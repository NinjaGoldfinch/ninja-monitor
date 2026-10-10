//! Builds one `Sample` per tick and publishes it on the bus. Knows nothing
//! about who consumes it.

pub mod cgroup;
pub mod disk;
pub mod host;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;
use tokio::time::MissedTickBehavior;
use tracing::debug;

use crate::config::Config;
use crate::sample::{HostSample, Sample, ServiceSample, ServiceState};
use cgroup::{CpuTracker, parse_keyed_u64, parse_single_u64};
use host::CpuTimes;

pub struct Sampler {
    host_name: String,
    proc_root: PathBuf,
    mounts: Vec<PathBuf>,
    prev_host_cpu: Option<CpuTimes>,
    services: Vec<ServiceSlot>,
}

struct ServiceSlot {
    name: String,
    unit: String,
    dir: PathBuf,
    cpu: CpuTracker,
}

impl Sampler {
    pub fn new(config: &Config) -> Self {
        let services = config
            .services
            .iter()
            .map(|s| ServiceSlot {
                name: s.name.clone(),
                unit: s.unit.clone(),
                dir: config.paths.cgroup_root.join(s.cgroup_rel()),
                cpu: CpuTracker::default(),
            })
            .collect();
        Self {
            host_name: config.host.name.clone(),
            proc_root: config.paths.proc_root.clone(),
            mounts: config.host.mounts.clone(),
            prev_host_cpu: None,
            services,
        }
    }

    pub fn sample(&mut self, now: Instant, timestamp: u64) -> Sample {
        let host = self.sample_host();
        let services = self
            .services
            .iter_mut()
            .map(|slot| sample_service(slot, now))
            .collect();
        Sample {
            timestamp,
            host,
            services,
        }
    }

    fn sample_host(&mut self) -> HostSample {
        let proc = &self.proc_root;
        let stat = read_file(&proc.join("stat")).map(|t| host::parse_proc_stat(&t));
        if stat.is_some_and(|s| s.cpu.is_none()) {
            debug!(path = %proc.join("stat").display(), "malformed content");
        }
        let cpu_now = stat.and_then(|s| s.cpu);
        let cpu_percent = match (self.prev_host_cpu, cpu_now) {
            (Some(prev), Some(cur)) => host::host_cpu_percent(prev, cur).map(round2),
            _ => None,
        };
        if cpu_now.is_some() {
            self.prev_host_cpu = cpu_now;
        }

        let load = read_parsed(&proc.join("loadavg"), host::parse_loadavg);
        let mem = read_parsed(&proc.join("meminfo"), host::parse_meminfo).unwrap_or_default();
        let uptime_secs = read_parsed(&proc.join("uptime"), host::parse_uptime);

        HostSample {
            name: self.host_name.clone(),
            cpu_percent,
            cores: stat.and_then(|s| s.cores),
            load1: load.map(|l| l.0),
            load5: load.map(|l| l.1),
            load15: load.map(|l| l.2),
            mem_total_bytes: mem.total,
            mem_available_bytes: mem.available,
            mem_used_bytes: mem.used(),
            mem_cache_bytes: mem.cache(),
            swap_total_bytes: mem.swap_total,
            swap_used_bytes: mem.swap_used(),
            uptime_secs,
            disks: self.mounts.iter().map(|m| disk::mount_usage(m)).collect(),
        }
    }
}

fn sample_service(slot: &mut ServiceSlot, now: Instant) -> ServiceSample {
    let mut sample = ServiceSample {
        name: slot.name.clone(),
        unit: slot.unit.clone(),
        state: ServiceState::Down,
        cpu_percent: None,
        mem_current_bytes: None,
        mem_anon_bytes: None,
        mem_file_bytes: None,
        mem_peak_bytes: None,
        oom_kills: None,
        pids: None,
        restarts_seen: slot.cpu.restarts_seen,
        disk_bytes: None,
        disk_measured_at: None,
    };
    if !slot.dir.is_dir() {
        return sample;
    }

    let dir = &slot.dir;
    let usage = read_parsed(&dir.join("cpu.stat"), |t| parse_keyed_u64(t, "usage_usec"));
    sample.cpu_percent = slot.cpu.update(usage, now).map(round2);
    sample.restarts_seen = slot.cpu.restarts_seen;

    sample.mem_current_bytes = read_parsed(&dir.join("memory.current"), parse_single_u64);
    if let Some(stat) = read_file(&dir.join("memory.stat")) {
        sample.mem_anon_bytes = parse_keyed_u64(&stat, "anon");
        sample.mem_file_bytes = parse_keyed_u64(&stat, "file");
        if sample.mem_anon_bytes.is_none() || sample.mem_file_bytes.is_none() {
            debug!(path = %dir.join("memory.stat").display(), "malformed content");
        }
    }
    sample.mem_peak_bytes = read_parsed(&dir.join("memory.peak"), parse_single_u64);
    sample.oom_kills = read_parsed(&dir.join("memory.events"), |t| {
        parse_keyed_u64(t, "oom_kill")
    });
    sample.pids = read_parsed(&dir.join("pids.current"), parse_single_u64);

    let running = match sample.pids {
        Some(pids) => pids > 0,
        // The pids controller can be disabled; fall back to the process list.
        None => read_file(&dir.join("cgroup.procs")).is_some_and(|t| !t.trim().is_empty()),
    };
    if running {
        sample.state = ServiceState::Running;
    }
    sample
}

/// Ticks forever, publishing each sample. Sending never waits on subscribers.
pub async fn run(mut sampler: Sampler, interval: Duration, bus: broadcast::Sender<Arc<Sample>>) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let sample = sampler.sample(Instant::now(), unix_millis());
        // An error only means nobody is subscribed right now.
        let _ = bus.send(Arc::new(sample));
    }
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Missing or unreadable files are expected (stopped services, old kernels).
fn read_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) => {
            debug!(path = %path.display(), %err, "cannot read");
            None
        }
    }
}

fn read_parsed<T>(path: &Path, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    let text = read_file(path)?;
    let value = parse(&text);
    if value.is_none() {
        debug!(path = %path.display(), "malformed content");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_config() -> Config {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let text = format!(
            r#"
            [paths]
            proc_root = "{proc}"
            cgroup_root = "{cgroup}"
            [host]
            name = "test"
            mounts = ["/"]
            [[service]]
            name = "cloudflared"
            unit = "cloudflared.service"
            [[service]]
            name = "riot-proxy"
            unit = "riot-proxy.service"
            [[service]]
            name = "site"
            unit = "site.service"
            "#,
            proc = root.join("proc").display(),
            cgroup = root.join("cgroup").display(),
        );
        Config::parse(&text).unwrap()
    }

    #[test]
    fn samples_fixture_tree() {
        let mut sampler = Sampler::new(&fixture_config());
        let s = sampler.sample(Instant::now(), 1);

        let h = &s.host;
        assert_eq!(h.name, "test");
        assert_eq!(h.cpu_percent, None);
        assert_eq!(h.cores, Some(2));
        assert_eq!(
            (h.load1, h.load5, h.load15),
            (Some(0.33), Some(0.27), Some(0.26))
        );
        assert_eq!(h.mem_total_bytes, Some(3906236 * 1024));
        assert_eq!(h.mem_available_bytes, Some(3167336 * 1024));
        assert_eq!(h.mem_used_bytes, Some((3906236 - 3167336) * 1024));
        assert_eq!(h.mem_cache_bytes, Some((2359700 + 200412 + 304020) * 1024));
        assert_eq!(h.swap_total_bytes, Some(0));
        assert_eq!(h.swap_used_bytes, Some(0));
        assert_eq!(h.uptime_secs, Some(18486));
        assert_eq!(h.disks.len(), 1);
        assert!(h.disks[0].total_bytes.is_some());

        let cf = &s.services[0];
        assert_eq!(cf.state, ServiceState::Running);
        assert_eq!(cf.cpu_percent, None);
        assert_eq!(cf.mem_current_bytes, Some(59006976));
        assert_eq!(cf.mem_anon_bytes, Some(16326656));
        assert_eq!(cf.mem_file_bytes, Some(41074688));
        assert_eq!(cf.mem_peak_bytes, Some(64307200));
        assert_eq!(cf.oom_kills, Some(0));
        assert_eq!(cf.pids, Some(8));
        assert_eq!(cf.disk_bytes, None);

        let riot = &s.services[1];
        assert_eq!(riot.state, ServiceState::Running);
        assert_eq!(riot.mem_anon_bytes, Some(208506880));
        assert_eq!(
            riot.mem_peak_bytes, None,
            "riot-proxy fixture has no memory.peak"
        );

        let site = &s.services[2];
        assert_eq!(site.state, ServiceState::Down);
        assert_eq!(site.mem_current_bytes, None);
        assert_eq!(site.pids, None);
    }

    #[test]
    fn missing_roots_give_nulls_not_panics() {
        let mut config = fixture_config();
        config.paths.proc_root = PathBuf::from("/definitely/not/proc");
        config.paths.cgroup_root = PathBuf::from("/definitely/not/cgroup");
        let mut sampler = Sampler::new(&config);
        let s = sampler.sample(Instant::now(), 1);
        assert_eq!(s.host.cores, None);
        assert_eq!(s.host.mem_total_bytes, None);
        assert_eq!(s.host.uptime_secs, None);
        assert!(s.services.iter().all(|svc| svc.state == ServiceState::Down));
    }

    #[test]
    fn round2_rounds() {
        assert_eq!(round2(12.3456), 12.35);
        assert_eq!(round2(0.0), 0.0);
    }
}
