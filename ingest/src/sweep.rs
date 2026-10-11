//! Retention: deletes old rows on a slow timer, in small batches so the
//! write lock is never held for long.

use std::time::Duration;

use tracing::{info, warn};

use ninja_monitor::sampler::unix_millis;

use crate::config::StorageConfig;
use crate::db::Db;
use crate::store;

/// First sweep after start, then every `INTERVAL`.
pub const FIRST_AFTER: Duration = Duration::from_secs(3600);
pub const INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Rows per DELETE statement.
pub const BATCH: usize = 10_000;
/// Breather between statements, so readers and the ingest route get a turn.
const PAUSE: Duration = Duration::from_millis(20);

pub async fn run(db: Db, config: StorageConfig) {
    run_with(db, config, FIRST_AFTER, INTERVAL).await
}

pub async fn run_with(db: Db, config: StorageConfig, first_after: Duration, interval: Duration) {
    tokio::time::sleep(first_after).await;
    loop {
        sweep_once(&db, &config).await;
        tokio::time::sleep(interval).await;
    }
}

pub async fn sweep_once(db: &Db, config: &StorageConfig) {
    let retention = config.retention.as_millis() as u64;
    let events_retention = config.events_retention.as_millis() as u64;
    let started = std::time::Instant::now();
    let result = db
        .call(move |c| {
            let swept = store::sweep(c, unix_millis(), retention, events_retention, BATCH, || {
                std::thread::sleep(PAUSE)
            })?;
            store::checkpoint(c)?;
            Ok(swept)
        })
        .await;
    match result {
        Ok(swept) => info!(
            samples = swept.samples,
            events = swept.events,
            took_ms = started.elapsed().as_millis() as u64,
            "retention sweep done"
        ),
        Err(err) => warn!(err = format!("{err:#}"), "retention sweep failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ninja_monitor::sample::{HostSample, Sample};

    fn sample(t: u64) -> Sample {
        Sample {
            timestamp: t,
            host: HostSample {
                name: "vps".into(),
                cpu_percent: None,
                cores: None,
                load1: None,
                load5: None,
                load15: None,
                mem_total_bytes: None,
                mem_available_bytes: None,
                mem_used_bytes: None,
                mem_cache_bytes: None,
                swap_total_bytes: None,
                swap_used_bytes: None,
                uptime_secs: None,
                disks: Vec::new(),
            },
            services: Vec::new(),
        }
    }

    #[tokio::test]
    async fn sweep_once_applies_the_configured_retention() {
        let db = Db::in_memory().unwrap();
        let config = Config::parse("[storage]\nretention = \"1h\"")
            .unwrap()
            .storage;
        let now = unix_millis();
        let samples = [
            sample(now - 2 * 3_600_000),
            sample(now - 60_000),
            sample(now),
        ];
        db.call_sync(|c| store::insert_batch(c, "vps", None, now, &samples, &[]))
            .unwrap();
        sweep_once(&db, &config).await;
        let host = db
            .call_sync(|c| store::host_by_name(c, "vps"))
            .unwrap()
            .unwrap();
        let (points, _) = db
            .call_sync(|c| store::host_points(c, host.id, 0, u64::MAX))
            .unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].t, now - 60_000);
    }
}
