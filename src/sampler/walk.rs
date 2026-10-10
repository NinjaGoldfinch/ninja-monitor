//! Slow per-service disk usage: sums allocated space under configured paths
//! on its own timer, off the async thread, and hands results to the sampler.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use super::unix_millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub bytes: u64,
    /// Unix milliseconds when the walk finished.
    pub measured_at: u64,
}

/// Latest result per configured service, in config order.
pub type DiskUsages = Vec<Option<DiskUsage>>;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub bytes: u64,
    pub errors: u64,
    pub first_error: Option<String>,
}

/// Allocated bytes under `root`. Symlinks below the root are counted as
/// links, never followed; other filesystems are skipped; hard-linked files
/// count once per `seen` set. Unreadable entries are skipped and counted.
/// Fails only if `root` itself cannot be read.
pub fn walk_path(
    root: &Path,
    seen: &mut HashSet<(u64, u64)>,
    stop: &AtomicBool,
) -> io::Result<WalkStats> {
    let root_meta = fs::metadata(root)?;
    let dev = root_meta.dev();
    let mut stats = WalkStats::default();
    add(&mut stats, &root_meta, seen);

    let mut dirs = Vec::new();
    if root_meta.is_dir() {
        dirs.push(root.to_path_buf());
    }
    while let Some(dir) = dirs.pop() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                note(&mut stats, &dir, err);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    note(&mut stats, &dir, err);
                    continue;
                }
            };
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    note(&mut stats, &path, err);
                    continue;
                }
            };
            if meta.dev() != dev {
                continue;
            }
            add(&mut stats, &meta, seen);
            if meta.is_dir() {
                dirs.push(path);
            }
        }
    }
    Ok(stats)
}

fn add(stats: &mut WalkStats, meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) {
    if !meta.is_dir() && meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
        return;
    }
    stats.bytes += meta.blocks() * 512;
}

fn note(stats: &mut WalkStats, path: &Path, err: io::Error) {
    stats.errors += 1;
    if stats.first_error.is_none() {
        stats.first_error = Some(format!("{}: {err}", path.display()));
    }
}

/// Total for one service across its paths. `None` when it has no paths or
/// none of them could be read. Warns once per path for the process lifetime.
pub fn measure_service(
    paths: &[PathBuf],
    warned: &mut HashSet<PathBuf>,
    stop: &AtomicBool,
) -> Option<u64> {
    let mut seen = HashSet::new();
    let mut total = None;
    for path in paths {
        match walk_path(path, &mut seen, stop) {
            Ok(stats) => {
                *total.get_or_insert(0) += stats.bytes;
                if stats.errors > 0 && warned.insert(path.clone()) {
                    warn!(
                        path = %path.display(),
                        errors = stats.errors,
                        first = stats.first_error.as_deref().unwrap_or(""),
                        "some entries could not be read; disk usage is a lower bound"
                    );
                }
            }
            Err(err) => {
                if warned.insert(path.clone()) {
                    warn!(path = %path.display(), %err, "cannot measure disk usage");
                }
            }
        }
    }
    total
}

/// Walks every service's paths each `interval` and publishes the results.
pub async fn run(
    services: Vec<Vec<PathBuf>>,
    interval: Duration,
    tx: watch::Sender<DiskUsages>,
    stop: Arc<AtomicBool>,
) {
    let services = Arc::new(services);
    let warned = Arc::new(Mutex::new(HashSet::new()));
    run_without_overlap(interval, move || {
        let mut warned = warned.lock().unwrap_or_else(|e| e.into_inner());
        let results = services
            .iter()
            .map(|paths| {
                let bytes = measure_service(paths, &mut warned, &stop)?;
                Some(DiskUsage {
                    bytes,
                    measured_at: unix_millis(),
                })
            })
            .collect();
        if !stop.load(Ordering::Relaxed) {
            tx.send_replace(results);
        }
    })
    .await;
}

/// Runs `job` on the blocking pool every `interval`, skipping a round if the
/// previous run has not finished.
async fn run_without_overlap<F>(interval: Duration, job: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let job = Arc::new(job);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current: Option<JoinHandle<()>> = None;
    loop {
        ticker.tick().await;
        if current.as_ref().is_some_and(|h| !h.is_finished()) {
            debug!("previous disk walk still running, skipping this round");
            continue;
        }
        let job = job.clone();
        current = Some(tokio::task::spawn_blocking(move || job()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::AtomicUsize;

    fn allocated(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    fn walk(root: &Path) -> WalkStats {
        walk_path(root, &mut HashSet::new(), &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn counts_allocated_size_of_everything() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("sub/b"), vec![1u8; 100_000]).unwrap();

        let expected = allocated(root)
            + allocated(&root.join("a"))
            + allocated(&root.join("sub"))
            + allocated(&root.join("sub/b"));
        let stats = walk(root);
        assert_eq!(stats.bytes, expected);
        assert_eq!(stats.errors, 0);
        assert!(stats.bytes >= 110_000);
    }

    #[test]
    fn sparse_files_count_allocated_not_apparent() {
        let dir = tempfile::tempdir().unwrap();
        let file = fs::File::create(dir.path().join("sparse")).unwrap();
        file.set_len(1 << 30).unwrap();
        let stats = walk(dir.path());
        assert!(
            stats.bytes < 1 << 20,
            "sparse file counted as {} bytes",
            stats.bytes
        );
    }

    #[test]
    fn does_not_follow_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("big"), vec![1u8; 500_000]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("link-dir")).unwrap();
        symlink(outside.path().join("big"), dir.path().join("link-file")).unwrap();

        let stats = walk(dir.path());
        assert!(stats.bytes < 500_000, "followed a symlink: {}", stats.bytes);
        assert_eq!(
            stats.bytes,
            allocated(dir.path())
                + allocated(&dir.path().join("link-dir"))
                + allocated(&dir.path().join("link-file"))
        );
    }

    #[test]
    fn follows_a_symlinked_root() {
        let real = tempfile::tempdir().unwrap();
        fs::write(real.path().join("f"), vec![1u8; 50_000]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("data");
        symlink(real.path(), &link).unwrap();
        assert_eq!(walk(&link).bytes, walk(real.path()).bytes);
    }

    #[test]
    fn hard_links_count_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::write(&a, vec![1u8; 200_000]).unwrap();
        fs::hard_link(&a, dir.path().join("b")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::hard_link(&a, dir.path().join("sub/c")).unwrap();
        let expected = allocated(dir.path()) + allocated(&a) + allocated(&dir.path().join("sub"));
        assert_eq!(walk(dir.path()).bytes, expected);
    }

    #[test]
    fn hard_links_count_once_across_paths_of_a_service() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        fs::write(one.path().join("a"), vec![1u8; 200_000]).unwrap();
        fs::hard_link(one.path().join("a"), two.path().join("a")).unwrap();
        let stop = AtomicBool::new(false);
        let total = measure_service(
            &[one.path().to_path_buf(), two.path().to_path_buf()],
            &mut HashSet::new(),
            &stop,
        )
        .unwrap();
        assert_eq!(total, walk(one.path()).bytes + allocated(two.path()));
    }

    #[test]
    fn unreadable_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok"), vec![1u8; 10_000]).unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden"), vec![1u8; 300_000]).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let stats = walk(dir.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if fs::read_dir(&locked).is_ok() && stats.errors == 0 {
            // Running as root: permissions are not enforced, nothing to check.
            return;
        }
        assert_eq!(stats.errors, 1);
        assert!(stats.first_error.unwrap().contains("locked"));
        assert!(stats.bytes >= 10_000 && stats.bytes < 300_000);
    }

    #[test]
    fn missing_paths() {
        let stop = AtomicBool::new(false);
        let mut warned = HashSet::new();
        let missing = PathBuf::from("/definitely/not/here");
        assert!(walk_path(&missing, &mut HashSet::new(), &stop).is_err());
        assert_eq!(
            measure_service(std::slice::from_ref(&missing), &mut warned, &stop),
            None
        );
        assert!(warned.contains(&missing));
        assert_eq!(measure_service(&[], &mut warned, &stop), None);

        let dir = tempfile::tempdir().unwrap();
        let some = measure_service(&[missing, dir.path().to_path_buf()], &mut warned, &stop);
        assert_eq!(some, Some(allocated(dir.path())));
    }

    #[test]
    fn stop_flag_ends_walk_early() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/f"), vec![1u8; 100_000]).unwrap();
        let stats = walk_path(dir.path(), &mut HashSet::new(), &AtomicBool::new(true)).unwrap();
        assert_eq!(stats.bytes, allocated(dir.path()));
    }

    #[tokio::test]
    async fn slow_walks_never_overlap() {
        let running = Arc::new(AtomicUsize::new(0));
        let max_running = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let (r, m, n) = (running.clone(), max_running.clone(), runs.clone());
        let task = tokio::spawn(run_without_overlap(Duration::from_millis(20), move || {
            let now = r.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(now, Ordering::SeqCst);
            n.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(70));
            r.fetch_sub(1, Ordering::SeqCst);
        }));
        tokio::time::sleep(Duration::from_millis(400)).await;
        task.abort();
        assert_eq!(max_running.load(Ordering::SeqCst), 1);
        let runs = runs.load(Ordering::SeqCst);
        assert!((2..=8).contains(&runs), "{runs} runs");
    }
}
