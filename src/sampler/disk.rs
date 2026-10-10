//! Filesystem usage via `statvfs`.

use std::path::Path;

use tracing::debug;

use crate::sample::DiskSample;
use crate::sampler::round2;

pub fn mount_usage(mount: &Path) -> DiskSample {
    let name = mount.display().to_string();
    match rustix::fs::statvfs(mount) {
        Ok(st) => usage_from_statvfs(name, st.f_blocks, st.f_bfree, st.f_bavail, st.f_frsize),
        Err(err) => {
            debug!(mount = %name, %err, "statvfs failed");
            DiskSample {
                mount: name,
                total_bytes: None,
                used_bytes: None,
                avail_bytes: None,
                used_percent: None,
            }
        }
    }
}

/// Matches `df`: used counts blocks not free to anyone, and the percentage is
/// taken against what unprivileged users can reach (used + avail).
pub fn usage_from_statvfs(
    mount: String,
    blocks: u64,
    bfree: u64,
    bavail: u64,
    frsize: u64,
) -> DiskSample {
    let total = blocks.saturating_mul(frsize);
    let used = blocks.saturating_sub(bfree).saturating_mul(frsize);
    let avail = bavail.saturating_mul(frsize);
    let reachable = used.saturating_add(avail);
    DiskSample {
        mount,
        total_bytes: Some(total),
        used_bytes: Some(used),
        avail_bytes: Some(avail),
        used_percent: (reachable > 0).then(|| round2(used as f64 / reachable as f64 * 100.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_maths() {
        let d = usage_from_statvfs("/".into(), 1000, 300, 250, 4096);
        assert_eq!(d.total_bytes, Some(1000 * 4096));
        assert_eq!(d.used_bytes, Some(700 * 4096));
        assert_eq!(d.avail_bytes, Some(250 * 4096));
        // 700 / 950 = 73.684...
        assert_eq!(d.used_percent, Some(73.68));
    }

    #[test]
    fn empty_filesystem_has_no_percent() {
        let d = usage_from_statvfs("/x".into(), 0, 0, 0, 4096);
        assert_eq!(d.used_percent, None);
    }

    #[test]
    fn real_and_missing_mounts() {
        let tmp = std::env::temp_dir();
        let d = mount_usage(&tmp);
        assert!(d.total_bytes.unwrap() > 0);
        let missing = mount_usage(Path::new("/definitely/not/a/mount"));
        assert_eq!(missing.total_bytes, None);
        assert_eq!(missing.used_percent, None);
    }
}
