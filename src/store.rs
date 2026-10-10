//! In-memory history: the latest sample plus fixed-capacity ring buffers of
//! compact points. Callers only see `latest()` and `range()`, so persistence
//! could be added later without touching the API.

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast::{self, error::RecvError};
use tracing::warn;

use crate::config::{Config, Scope};
use crate::sample::{Sample, ServiceState};

/// Fixed-capacity ring buffer. Allocates its full capacity once, up front.
pub struct Ring<T> {
    buf: Vec<T>,
    cap: usize,
    next: usize,
}

impl<T: Copy> Ring<T> {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            next: 0,
        }
    }

    pub fn push(&mut self, value: T) {
        if self.buf.len() < self.cap {
            self.buf.push(value);
        } else {
            self.buf[self.next] = value;
        }
        self.next = (self.next + 1) % self.cap;
    }

    /// Oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        let (newer, older) = self.buf.split_at(self.next);
        older.iter().chain(newer)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// Missing values are stored as NaN or MAX so points stay small and `Copy`.
pub fn pack_f32(value: Option<f64>) -> f32 {
    value.map_or(f32::NAN, |v| v as f32)
}

pub fn unpack_f32(value: f32) -> Option<f64> {
    (!value.is_nan()).then_some(value as f64)
}

pub fn pack_u64(value: Option<u64>) -> u64 {
    value.unwrap_or(u64::MAX)
}

pub fn unpack_u64(value: u64) -> Option<u64> {
    (value != u64::MAX).then_some(value)
}

pub fn pack_u32(value: Option<u64>) -> u32 {
    value.map_or(u32::MAX, |v| v.min(u32::MAX as u64 - 1) as u32)
}

pub fn unpack_u32(value: u32) -> Option<u64> {
    (value != u32::MAX).then_some(value as u64)
}

pub trait Timestamped {
    fn t(&self) -> u64;
}

#[derive(Debug, Clone, Copy)]
pub struct HostPoint {
    pub t: u64,
    pub cpu_percent: f32,
    pub load1: f32,
    pub load5: f32,
    pub load15: f32,
    pub mem_available_bytes: u64,
    pub mem_used_bytes: u64,
    pub mem_cache_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct DiskPoint {
    pub t: u64,
    pub used_bytes: u64,
    pub used_percent: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ServicePoint {
    pub t: u64,
    pub up: bool,
    pub cpu_percent: f32,
    pub pids: u32,
    pub mem_current_bytes: u64,
    pub mem_anon_bytes: u64,
    pub mem_file_bytes: u64,
    pub oom_kills: u64,
    pub disk_bytes: u64,
}

impl Timestamped for HostPoint {
    fn t(&self) -> u64 {
        self.t
    }
}

impl Timestamped for DiskPoint {
    fn t(&self) -> u64 {
        self.t
    }
}

impl Timestamped for ServicePoint {
    fn t(&self) -> u64 {
        self.t
    }
}

/// Points inside a requested window, oldest first.
pub enum Range {
    Host {
        points: Vec<HostPoint>,
        disks: Vec<(String, Vec<DiskPoint>)>,
    },
    Service(Vec<ServicePoint>),
}

pub struct Store {
    inner: RwLock<Inner>,
}

struct Inner {
    latest: Option<Arc<Sample>>,
    host: Ring<HostPoint>,
    disks: Vec<(String, Ring<DiskPoint>)>,
    services: Vec<(String, Ring<ServicePoint>)>,
}

impl Store {
    pub fn new(config: &Config) -> Self {
        let cap = capacity(config);
        Self {
            inner: RwLock::new(Inner {
                latest: None,
                host: Ring::new(cap),
                disks: config
                    .host
                    .mounts
                    .iter()
                    .map(|m| (m.display().to_string(), Ring::new(cap)))
                    .collect(),
                services: config
                    .services
                    .iter()
                    .map(|s| (s.name.clone(), Ring::new(cap)))
                    .collect(),
            }),
        }
    }

    pub fn latest(&self) -> Option<Arc<Sample>> {
        self.read().latest.clone()
    }

    pub fn push(&self, sample: Arc<Sample>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let t = sample.timestamp;
        let h = &sample.host;
        inner.host.push(HostPoint {
            t,
            cpu_percent: pack_f32(h.cpu_percent),
            load1: pack_f32(h.load1),
            load5: pack_f32(h.load5),
            load15: pack_f32(h.load15),
            mem_available_bytes: pack_u64(h.mem_available_bytes),
            mem_used_bytes: pack_u64(h.mem_used_bytes),
            mem_cache_bytes: pack_u64(h.mem_cache_bytes),
            swap_used_bytes: pack_u64(h.swap_used_bytes),
        });
        for (mount, ring) in &mut inner.disks {
            let disk = h.disks.iter().find(|d| d.mount == *mount);
            ring.push(DiskPoint {
                t,
                used_bytes: pack_u64(disk.and_then(|d| d.used_bytes)),
                used_percent: pack_f32(disk.and_then(|d| d.used_percent)),
            });
        }
        for (name, ring) in &mut inner.services {
            let Some(s) = sample.services.iter().find(|s| s.name == *name) else {
                continue;
            };
            ring.push(ServicePoint {
                t,
                up: s.state == ServiceState::Running,
                cpu_percent: pack_f32(s.cpu_percent),
                pids: pack_u32(s.pids),
                mem_current_bytes: pack_u64(s.mem_current_bytes),
                mem_anon_bytes: pack_u64(s.mem_anon_bytes),
                mem_file_bytes: pack_u64(s.mem_file_bytes),
                oom_kills: pack_u64(s.oom_kills),
                disk_bytes: pack_u64(s.disk_bytes),
            });
        }
        inner.latest = Some(sample);
    }

    /// Points with `from <= t <= to`. `None` means the scope is unknown.
    pub fn range(&self, scope: &Scope, from: u64, to: u64) -> Option<Range> {
        let inner = self.read();
        match scope {
            Scope::Host => Some(Range::Host {
                points: window(&inner.host, from, to),
                disks: inner
                    .disks
                    .iter()
                    .map(|(mount, ring)| (mount.clone(), window(ring, from, to)))
                    .collect(),
            }),
            Scope::Service(name) => inner
                .services
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ring)| Range::Service(window(ring, from, to))),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// Enough slots to cover `history` at one sample per `interval`.
pub fn capacity(config: &Config) -> usize {
    let history = config.sampling.history.as_millis();
    let interval = config.sampling.interval.as_millis().max(1);
    history.div_ceil(interval).max(1) as usize
}

fn window<T: Copy + Timestamped>(ring: &Ring<T>, from: u64, to: u64) -> Vec<T> {
    ring.iter()
        .filter(|p| (from..=to).contains(&p.t()))
        .copied()
        .collect()
}

/// Bus subscriber that feeds the store.
pub async fn run(store: Arc<Store>, mut rx: broadcast::Receiver<Arc<Sample>>) {
    loop {
        match rx.recv().await {
            Ok(sample) => store.push(sample),
            Err(RecvError::Lagged(missed)) => warn!(missed, "store fell behind the sampler"),
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{DiskSample, HostSample, ServiceSample};

    fn config() -> Config {
        Config::parse(
            r#"
            [sampling]
            interval = "1s"
            history = "5s"
            [host]
            mounts = ["/"]
            [[service]]
            name = "a"
            unit = "a.service"
            "#,
        )
        .unwrap()
    }

    fn sample(t: u64, cpu: Option<f64>, anon: Option<u64>) -> Arc<Sample> {
        Arc::new(Sample {
            timestamp: t,
            host: HostSample {
                name: "h".into(),
                cpu_percent: cpu,
                cores: Some(2),
                load1: Some(0.5),
                load5: None,
                load15: None,
                mem_total_bytes: Some(100),
                mem_available_bytes: Some(60),
                mem_used_bytes: Some(40),
                mem_cache_bytes: None,
                swap_total_bytes: Some(0),
                swap_used_bytes: Some(0),
                uptime_secs: Some(1),
                disks: vec![DiskSample {
                    mount: "/".into(),
                    total_bytes: Some(10),
                    used_bytes: Some(t),
                    avail_bytes: Some(5),
                    used_percent: Some(50.0),
                }],
            },
            services: vec![ServiceSample {
                name: "a".into(),
                unit: "a.service".into(),
                state: ServiceState::Running,
                cpu_percent: cpu,
                mem_current_bytes: None,
                mem_anon_bytes: anon,
                mem_file_bytes: None,
                mem_peak_bytes: None,
                oom_kills: Some(0),
                pids: Some(3),
                restarts_seen: 0,
                disk_bytes: None,
                disk_measured_at: None,
            }],
        })
    }

    #[test]
    fn ring_wraps_oldest_first() {
        let mut ring = Ring::new(3);
        assert!(ring.is_empty());
        for v in 1..=2 {
            ring.push(v);
        }
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [1, 2]);
        for v in 3..=7 {
            ring.push(v);
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [5, 6, 7]);
    }

    #[test]
    fn ring_of_one() {
        let mut ring = Ring::new(0);
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn packing_round_trips() {
        assert_eq!(unpack_f32(pack_f32(None)), None);
        assert_eq!(unpack_f32(pack_f32(Some(12.5))), Some(12.5));
        assert_eq!(unpack_u64(pack_u64(None)), None);
        assert_eq!(unpack_u64(pack_u64(Some(7))), Some(7));
        assert_eq!(unpack_u32(pack_u32(None)), None);
        assert_eq!(unpack_u32(pack_u32(Some(9))), Some(9));
    }

    #[test]
    fn capacity_from_history_and_interval() {
        assert_eq!(capacity(&config()), 5);
        let day = Config::parse("[sampling]\ninterval = \"15s\"\nhistory = \"24h\"").unwrap();
        assert_eq!(capacity(&day), 5760);
        let odd = Config::parse("[sampling]\ninterval = \"7s\"\nhistory = \"1m\"").unwrap();
        assert_eq!(capacity(&odd), 9);
    }

    #[test]
    fn keeps_latest_and_bounded_history() {
        let store = Store::new(&config());
        assert!(store.latest().is_none());
        for t in 1..=8 {
            store.push(sample(t * 1000, Some(t as f64), Some(t)));
        }
        assert_eq!(store.latest().unwrap().timestamp, 8000);

        let Some(Range::Host { points, disks }) = store.range(&Scope::Host, 0, u64::MAX) else {
            panic!("host range missing");
        };
        let ts: Vec<u64> = points.iter().map(|p| p.t).collect();
        assert_eq!(ts, [4000, 5000, 6000, 7000, 8000]);
        assert_eq!(unpack_f32(points[0].cpu_percent), Some(4.0));
        assert_eq!(unpack_f32(points[0].load5), None);
        assert_eq!(unpack_u64(points[0].mem_cache_bytes), None);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].0, "/");
        assert_eq!(unpack_u64(disks[0].1[0].used_bytes), Some(4000));
    }

    #[test]
    fn range_filters_window_inclusive() {
        let store = Store::new(&config());
        for t in 1..=5 {
            store.push(sample(t * 1000, None, Some(t)));
        }
        let Some(Range::Service(points)) = store.range(&Scope::Service("a".into()), 2000, 4000)
        else {
            panic!("service range missing");
        };
        let anon: Vec<_> = points
            .iter()
            .map(|p| unpack_u64(p.mem_anon_bytes))
            .collect();
        assert_eq!(anon, [Some(2), Some(3), Some(4)]);
        assert!(points.iter().all(|p| p.up && unpack_u32(p.pids) == Some(3)));
        assert!(points.iter().all(|p| unpack_f32(p.cpu_percent).is_none()));

        let Some(Range::Service(empty)) = store.range(&Scope::Service("a".into()), 9000, 9999)
        else {
            panic!("service range missing");
        };
        assert!(empty.is_empty());
    }

    #[test]
    fn day_of_history_fits_the_budget() {
        // Host, one mount and five services at 15 s for 24 h must stay well under 5 MiB.
        let slots = 24 * 60 * 60 / 15;
        let bytes = slots
            * (size_of::<HostPoint>() + size_of::<DiskPoint>() + 5 * size_of::<ServicePoint>());
        assert!(bytes < 3 * 1024 * 1024, "{bytes} bytes");
    }

    #[test]
    fn unknown_service_scope() {
        let store = Store::new(&config());
        assert!(store.range(&Scope::Service("nope".into()), 0, 1).is_none());
    }
}
