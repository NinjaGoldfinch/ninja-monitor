//! Parsers and CPU accounting for per-service cgroup v2 directories.

use std::time::Instant;

/// Files holding a single number, such as `memory.current` or `pids.current`.
pub fn parse_single_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Flat keyed files such as `cpu.stat`, `memory.stat` and `memory.events`.
pub fn parse_keyed_u64(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        if k == key {
            v.trim().parse().ok()
        } else {
            None
        }
    })
}

/// Turns the cumulative `usage_usec` counter into percent of one core.
#[derive(Debug, Default)]
pub struct CpuTracker {
    prev: Option<(u64, Instant)>,
    pub restarts_seen: u64,
}

impl CpuTracker {
    /// A missing reading keeps the previous one, so a service that comes back
    /// with a lower counter is still recognised as restarted.
    pub fn update(&mut self, usage_usec: Option<u64>, now: Instant) -> Option<f64> {
        let usage = usage_usec?;
        let (prev_usage, prev_at) = self.prev.replace((usage, now))?;
        if usage < prev_usage {
            self.restarts_seen += 1;
            return None;
        }
        let elapsed_us = now.checked_duration_since(prev_at)?.as_micros();
        if elapsed_us == 0 {
            return None;
        }
        Some((usage - prev_usage) as f64 / elapsed_us as f64 * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const CPU_STAT: &str =
        include_str!("../../tests/fixtures/cgroup/system.slice/cloudflared.service/cpu.stat");
    const MEMORY_STAT: &str =
        include_str!("../../tests/fixtures/cgroup/system.slice/cloudflared.service/memory.stat");
    const MEMORY_EVENTS: &str =
        include_str!("../../tests/fixtures/cgroup/system.slice/cloudflared.service/memory.events");

    #[test]
    fn fixture_files() {
        assert_eq!(parse_keyed_u64(CPU_STAT, "usage_usec"), Some(54669841));
        assert_eq!(parse_keyed_u64(MEMORY_STAT, "anon"), Some(16326656));
        assert_eq!(parse_keyed_u64(MEMORY_STAT, "file"), Some(41074688));
        assert_eq!(parse_keyed_u64(MEMORY_EVENTS, "oom_kill"), Some(0));
        // `oom_kill` must not match `oom_group_kill`, `anon` must not match `anon_thp`.
        assert_eq!(
            parse_keyed_u64("oom_group_kill 5\noom_kill 2", "oom_kill"),
            Some(2)
        );
        assert_eq!(parse_keyed_u64("anon_thp 9\nanon 1", "anon"), Some(1));
    }

    #[test]
    fn keyed_missing_and_malformed() {
        assert_eq!(parse_keyed_u64("", "anon"), None);
        assert_eq!(parse_keyed_u64("file 10", "anon"), None);
        assert_eq!(parse_keyed_u64("anon lots", "anon"), None);
        assert_eq!(parse_keyed_u64("anon", "anon"), None);
    }

    #[test]
    fn single_values() {
        assert_eq!(parse_single_u64("59006976\n"), Some(59006976));
        assert_eq!(parse_single_u64("max\n"), None);
        assert_eq!(parse_single_u64(""), None);
    }

    #[test]
    fn cpu_first_sample_is_null() {
        let mut t = CpuTracker::default();
        assert_eq!(t.update(Some(1_000_000), Instant::now()), None);
        assert_eq!(t.restarts_seen, 0);
    }

    #[test]
    fn cpu_percent_of_one_core() {
        let mut t = CpuTracker::default();
        let start = Instant::now();
        t.update(Some(1_000_000), start);
        // 1.5 s of CPU in 1 s of wall time is 150 % of one core.
        let pct = t.update(Some(2_500_000), start + Duration::from_secs(1));
        assert_eq!(pct, Some(150.0));
        let pct = t.update(Some(2_500_000), start + Duration::from_secs(2));
        assert_eq!(pct, Some(0.0));
    }

    #[test]
    fn cpu_counter_reset_counts_restart() {
        let mut t = CpuTracker::default();
        let start = Instant::now();
        t.update(Some(5_000_000), start);
        assert_eq!(t.update(Some(100), start + Duration::from_secs(1)), None);
        assert_eq!(t.restarts_seen, 1);
        // The next sample measures from the new baseline.
        let pct = t.update(Some(500_100), start + Duration::from_secs(2));
        assert_eq!(pct, Some(50.0));
    }

    #[test]
    fn cpu_missing_reading_keeps_baseline() {
        let mut t = CpuTracker::default();
        let start = Instant::now();
        t.update(Some(5_000_000), start);
        assert_eq!(t.update(None, start + Duration::from_secs(1)), None);
        assert_eq!(t.update(Some(10), start + Duration::from_secs(2)), None);
        assert_eq!(t.restarts_seen, 1);
    }

    #[test]
    fn cpu_zero_elapsed_is_null() {
        let mut t = CpuTracker::default();
        let start = Instant::now();
        t.update(Some(1), start);
        assert_eq!(t.update(Some(2), start), None);
    }
}
