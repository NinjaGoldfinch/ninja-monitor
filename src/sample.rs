//! The immutable snapshot the sampler publishes on the bus every tick.
//!
//! These types are also the wire format pushed to ninja-ingest, which is why
//! they deserialize too. Adding a field is fine; renaming or removing one is
//! a protocol change.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Unix milliseconds, UTC.
    pub timestamp: u64,
    pub host: HostSample,
    pub services: Vec<ServiceSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSample {
    pub name: String,
    /// Busy time across all cores, 0 to 100. `null` on the first sample.
    pub cpu_percent: Option<f64>,
    pub cores: Option<u32>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub mem_used_bytes: Option<u64>,
    pub mem_cache_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub disks: Vec<DiskSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSample {
    pub mount: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub avail_bytes: Option<u64>,
    pub used_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Running,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSample {
    pub name: String,
    pub unit: String,
    pub state: ServiceState,
    /// Percent of one core; may exceed 100. `null` on the first sample and after a restart.
    pub cpu_percent: Option<f64>,
    pub mem_current_bytes: Option<u64>,
    pub mem_anon_bytes: Option<u64>,
    pub mem_file_bytes: Option<u64>,
    pub mem_peak_bytes: Option<u64>,
    pub oom_kills: Option<u64>,
    pub pids: Option<u64>,
    pub restarts_seen: u64,
    pub disk_bytes: Option<u64>,
    /// Unix milliseconds of the walk that produced `disk_bytes`.
    pub disk_measured_at: Option<u64>,
}
