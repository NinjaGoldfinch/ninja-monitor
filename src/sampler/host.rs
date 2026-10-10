//! Parsers for the host-wide files under `proc_root`.

/// Aggregate CPU jiffies from the first line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    /// `idle + iowait`.
    pub idle: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    pub cpu: Option<CpuTimes>,
    pub cores: Option<u32>,
}

pub fn parse_proc_stat(text: &str) -> ProcStat {
    let mut cpu = None;
    let mut cores = 0u32;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("cpu") => cpu = parse_cpu_fields(fields),
            Some(name) if is_core_label(name) => cores += 1,
            _ => {}
        }
    }
    ProcStat {
        cpu,
        cores: (cores > 0).then_some(cores),
    }
}

fn is_core_label(name: &str) -> bool {
    name.strip_prefix("cpu")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Sums user, nice, system, idle, iowait, irq, softirq and steal. Guest time is
/// already counted inside user and nice, so the last two columns are skipped.
fn parse_cpu_fields<'a>(fields: impl Iterator<Item = &'a str>) -> Option<CpuTimes> {
    let values: Vec<u64> = fields
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if values.len() < 4 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        total: values.iter().sum(),
        idle,
    })
}

/// Busy percentage across all cores between two readings, 0 to 100.
pub fn host_cpu_percent(prev: CpuTimes, cur: CpuTimes) -> Option<f64> {
    let total = cur.total.checked_sub(prev.total)?;
    if total == 0 {
        return None;
    }
    let busy_prev = prev.total.saturating_sub(prev.idle);
    let busy_cur = cur.total.saturating_sub(cur.idle);
    let busy = busy_cur.saturating_sub(busy_prev);
    Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

pub fn parse_loadavg(text: &str) -> Option<(f64, f64, f64)> {
    let mut fields = text.split_whitespace().map(|f| f.parse::<f64>().ok());
    Some((fields.next()??, fields.next()??, fields.next()??))
}

/// The `/proc/meminfo` fields we use, converted to bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: Option<u64>,
    pub available: Option<u64>,
    pub buffers: Option<u64>,
    pub cached: Option<u64>,
    pub sreclaimable: Option<u64>,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
}

impl MemInfo {
    pub fn used(&self) -> Option<u64> {
        Some(self.total?.saturating_sub(self.available?))
    }

    pub fn cache(&self) -> Option<u64> {
        Some(self.cached? + self.buffers? + self.sreclaimable?)
    }

    pub fn swap_used(&self) -> Option<u64> {
        Some(self.swap_total?.saturating_sub(self.swap_free?))
    }
}

/// Returns `None` when no line could be parsed at all.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut info = MemInfo::default();
    let mut any = false;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "MemTotal" => &mut info.total,
            "MemAvailable" => &mut info.available,
            "Buffers" => &mut info.buffers,
            "Cached" => &mut info.cached,
            "SReclaimable" => &mut info.sreclaimable,
            "SwapTotal" => &mut info.swap_total,
            "SwapFree" => &mut info.swap_free,
            _ => continue,
        };
        if let Some(bytes) = parse_meminfo_value(rest) {
            *slot = Some(bytes);
            any = true;
        }
    }
    any.then_some(info)
}

fn parse_meminfo_value(rest: &str) -> Option<u64> {
    let mut parts = rest.split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => value.checked_mul(1024),
        None => Some(value),
        Some(_) => None,
    }
}

pub fn parse_uptime(text: &str) -> Option<u64> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = include_str!("../../tests/fixtures/proc/stat");
    const MEMINFO: &str = include_str!("../../tests/fixtures/proc/meminfo");

    #[test]
    fn proc_stat_fixture() {
        let stat = parse_proc_stat(STAT);
        assert_eq!(stat.cores, Some(2));
        // cpu  531316 0 82729 3055641 6002 0 2500 775 0 0
        assert_eq!(
            stat.cpu,
            Some(CpuTimes {
                total: 531316 + 82729 + 3055641 + 6002 + 2500 + 775,
                idle: 3055641 + 6002,
            })
        );
    }

    #[test]
    fn proc_stat_malformed_and_short() {
        assert_eq!(
            parse_proc_stat(""),
            ProcStat {
                cpu: None,
                cores: None
            }
        );
        assert_eq!(parse_proc_stat("cpu  1 2 x 4\ncpu0 1 2 3 4").cpu, None);
        assert_eq!(parse_proc_stat("cpu 1 2 3").cpu, None);
        // Old kernels only have four columns.
        assert_eq!(
            parse_proc_stat("cpu 10 0 10 80").cpu,
            Some(CpuTimes {
                total: 100,
                idle: 80
            })
        );
        // "cpufreq"-style labels are not cores.
        assert_eq!(
            parse_proc_stat("cpux 1\ncpu 1 1 1 1\ncpu0 1").cores,
            Some(1)
        );
    }

    #[test]
    fn host_cpu_delta() {
        let a = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let b = CpuTimes {
            total: 1200,
            idle: 950,
        };
        // 200 jiffies passed, 150 idle, 50 busy.
        assert_eq!(host_cpu_percent(a, b), Some(25.0));
        assert_eq!(host_cpu_percent(a, a), None);
        assert_eq!(host_cpu_percent(b, a), None);
        // Idle going backwards (seen on some kernels) never yields above 100.
        let c = CpuTimes {
            total: 1100,
            idle: 700,
        };
        assert_eq!(host_cpu_percent(a, c), Some(100.0));
    }

    #[test]
    fn loadavg() {
        assert_eq!(
            parse_loadavg("0.33 0.27 0.26 3/177 5782\n"),
            Some((0.33, 0.27, 0.26))
        );
        assert_eq!(parse_loadavg("0.33 0.27"), None);
        assert_eq!(parse_loadavg("a b c"), None);
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn meminfo_fixture() {
        let info = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(info.total, Some(3906236 * 1024));
        assert_eq!(info.available, Some(3167336 * 1024));
        assert_eq!(info.used(), Some((3906236 - 3167336) * 1024));
        assert_eq!(info.cache(), Some((2359700 + 200412 + 304020) * 1024));
        assert_eq!(info.swap_total, Some(0));
        assert_eq!(info.swap_used(), Some(0));
    }

    #[test]
    fn meminfo_malformed_and_partial() {
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("garbage\nMemTotal: lots kB"), None);
        let info = parse_meminfo("MemTotal: 100 kB\nMemAvailable: 40 MB").unwrap();
        assert_eq!(info.total, Some(102400));
        assert_eq!(info.available, None);
        assert_eq!(info.used(), None);
        assert_eq!(info.cache(), None);
    }

    #[test]
    fn uptime() {
        assert_eq!(parse_uptime("18486.95 30556.41\n"), Some(18486));
        assert_eq!(parse_uptime("nope"), None);
        assert_eq!(parse_uptime("-5 1"), None);
        assert_eq!(parse_uptime(""), None);
    }
}
