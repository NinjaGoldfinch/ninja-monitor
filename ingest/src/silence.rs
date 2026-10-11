//! Notices hosts that have stopped reporting and tells Discord.
//!
//! `Watcher::evaluate` is pure: it looks at the host rows and the clock and
//! says what changed. `run` applies the changes to the database and the
//! notification queue on a timer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use ninja_monitor::alerts::notify::{Queue, human_duration};
use ninja_monitor::sampler::unix_millis;

use crate::config::SilenceConfig;
use crate::db::Db;
use crate::store::{self, HostRow};

/// Longest time between checks. Shorter `after` values check more often.
pub const MAX_TICK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Silent,
    Repeat,
    Reporting,
}

/// One change the watcher wants applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub host_id: i64,
    pub host: String,
    pub kind: Kind,
    pub message: String,
    /// New value for `host.silent_since`; `None` means leave it alone.
    pub set_silent_since: Option<Option<u64>>,
}

pub struct Watcher {
    after_ms: u64,
    repeat_ms: Option<u64>,
    /// Last time SILENT was posted for each host, for `repeat`.
    last_posted: HashMap<String, u64>,
}

impl Watcher {
    pub fn new(config: &SilenceConfig) -> Self {
        Self {
            after_ms: config.after.as_millis() as u64,
            repeat_ms: config.repeat.map(|d| d.as_millis() as u64),
            last_posted: HashMap::new(),
        }
    }

    /// How often to check.
    pub fn tick(&self) -> Duration {
        Duration::from_millis(self.after_ms / 2).clamp(Duration::from_millis(10), MAX_TICK)
    }

    pub fn evaluate(&mut self, rows: &[HostRow], now: u64) -> Vec<Transition> {
        let mut out = Vec::new();
        for row in rows {
            let quiet_for = now.saturating_sub(row.last_seen);
            match row.silent_since {
                None if quiet_for >= self.after_ms => {
                    let since = row.last_seen + self.after_ms;
                    self.last_posted.insert(row.name.clone(), now);
                    out.push(Transition {
                        host_id: row.id,
                        host: row.name.clone(),
                        kind: Kind::Silent,
                        message: format!(
                            "**SILENT** [{}] no samples for {}, last seen {}",
                            row.name,
                            human_duration(quiet_for / 1000),
                            format_utc(row.last_seen)
                        ),
                        set_silent_since: Some(Some(since)),
                    });
                }
                None => {}
                Some(since) if quiet_for < self.after_ms => {
                    self.last_posted.remove(&row.name);
                    out.push(Transition {
                        host_id: row.id,
                        host: row.name.clone(),
                        kind: Kind::Reporting,
                        message: format!(
                            "**REPORTING** [{}] back after {}",
                            row.name,
                            human_duration(row.last_seen.saturating_sub(since) / 1000)
                        ),
                        set_silent_since: Some(None),
                    });
                }
                Some(_) => {
                    // Still silent. A host found already silent at startup
                    // starts its repeat clock now rather than firing at once.
                    let last = *self.last_posted.entry(row.name.clone()).or_insert(now);
                    if let Some(repeat) = self.repeat_ms
                        && now.saturating_sub(last) >= repeat
                    {
                        self.last_posted.insert(row.name.clone(), now);
                        out.push(Transition {
                            host_id: row.id,
                            host: row.name.clone(),
                            kind: Kind::Repeat,
                            message: format!(
                                "**SILENT** [{}] still no samples after {}, last seen {}",
                                row.name,
                                human_duration(quiet_for / 1000),
                                format_utc(row.last_seen)
                            ),
                            set_silent_since: None,
                        });
                    }
                }
            }
        }
        out
    }
}

/// `2026-10-11 14:02:11 UTC` from Unix milliseconds, without a date crate.
pub fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Checks on a timer until the task is aborted.
pub async fn run(db: Db, config: SilenceConfig, queue: Option<Arc<Queue>>) {
    let mut watcher = Watcher::new(&config);
    let mut ticker = tokio::time::interval(watcher.tick());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let rows = match db.call(|c| store::hosts(c)).await {
            Ok(rows) => rows,
            Err(err) => {
                warn!(
                    err = format!("{err:#}"),
                    "silence watcher cannot read hosts"
                );
                continue;
            }
        };
        for t in watcher.evaluate(&rows, unix_millis()) {
            if let Some(value) = t.set_silent_since {
                let id = t.host_id;
                if let Err(err) = db
                    .call(move |c| store::set_silent_since(c, id, value))
                    .await
                {
                    warn!(
                        err = format!("{err:#}"),
                        host = t.host,
                        "cannot update silent_since"
                    );
                }
            }
            match t.kind {
                Kind::Silent => info!(host = t.host, "host went silent"),
                Kind::Repeat => debug!(host = t.host, "host still silent"),
                Kind::Reporting => info!(host = t.host, "host reporting again"),
            }
            match &queue {
                Some(queue) => queue.push(t.message),
                None => warn!("{}", t.message),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;
    const M: u64 = 60 * S;

    fn config(after: &str, repeat: Option<&str>) -> SilenceConfig {
        let repeat = repeat.map_or(String::new(), |r| format!("repeat = \"{r}\""));
        crate::config::Config::parse(&format!("[silence]\nafter = \"{after}\"\n{repeat}"))
            .unwrap()
            .silence
    }

    fn row(name: &str, last_seen: u64, silent_since: Option<u64>) -> HostRow {
        HostRow {
            id: 1,
            name: name.into(),
            first_seen: 0,
            last_seen,
            silent_since,
            agent: None,
        }
    }

    #[test]
    fn tick_follows_after() {
        assert_eq!(
            Watcher::new(&config("2m", None)).tick(),
            Duration::from_secs(30)
        );
        assert_eq!(
            Watcher::new(&config("20s", None)).tick(),
            Duration::from_secs(10)
        );
        assert_eq!(
            Watcher::new(&config("1ms", None)).tick(),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn silent_then_reporting() {
        let mut w = Watcher::new(&config("2m", None));
        let seen = 1_760_000_000_000;
        assert!(w.evaluate(&[row("vps", seen, None)], seen + M).is_empty());
        assert!(
            w.evaluate(&[row("vps", seen, None)], seen + 2 * M - 1)
                .is_empty()
        );

        let t = w.evaluate(&[row("vps", seen, None)], seen + 2 * M + 10 * S);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, Kind::Silent);
        assert_eq!(t[0].set_silent_since, Some(Some(seen + 2 * M)));
        assert_eq!(
            t[0].message,
            format!(
                "**SILENT** [vps] no samples for 2m 10s, last seen {}",
                format_utc(seen)
            )
        );

        // Persisted as silent: nothing more without `repeat`, however long.
        let silent = row("vps", seen, Some(seen + 2 * M));
        assert!(
            w.evaluate(std::slice::from_ref(&silent), seen + 10 * M)
                .is_empty()
        );
        assert!(w.evaluate(&[silent], seen + 600 * M).is_empty());

        // A sample arrives: last_seen moves, silent_since still set.
        let back = seen + 14 * M + 40 * S;
        let t = w.evaluate(&[row("vps", back, Some(seen + 2 * M))], back + S);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, Kind::Reporting);
        assert_eq!(t[0].set_silent_since, Some(None));
        assert_eq!(t[0].message, "**REPORTING** [vps] back after 12m 40s");
        assert!(w.evaluate(&[row("vps", back, None)], back + S).is_empty());
    }

    #[test]
    fn repeat_while_silent_and_restart_does_not_refire() {
        let mut w = Watcher::new(&config("2m", Some("10m")));
        let seen = 1_760_000_000_000;
        let went_silent = seen + 3 * M;
        let t = w.evaluate(&[row("vps", seen, None)], went_silent);
        assert_eq!(t[0].kind, Kind::Silent);
        let silent = row("vps", seen, Some(seen + 2 * M));
        assert!(
            w.evaluate(std::slice::from_ref(&silent), went_silent + 9 * M)
                .is_empty()
        );
        let t = w.evaluate(std::slice::from_ref(&silent), went_silent + 10 * M);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, Kind::Repeat);
        assert!(
            t[0].message
                .starts_with("**SILENT** [vps] still no samples after 13m"),
            "{}",
            t[0].message
        );
        assert!(
            w.evaluate(std::slice::from_ref(&silent), went_silent + 15 * M)
                .is_empty()
        );
        assert_eq!(
            w.evaluate(std::slice::from_ref(&silent), went_silent + 20 * M)
                .len(),
            1
        );

        // A fresh watcher (server restart) finds the host already silent:
        // no SILENT again, and the first repeat comes a full interval later.
        let mut fresh = Watcher::new(&config("2m", Some("10m")));
        let start = went_silent + 30 * M;
        assert!(
            fresh
                .evaluate(std::slice::from_ref(&silent), start)
                .is_empty()
        );
        assert!(
            fresh
                .evaluate(std::slice::from_ref(&silent), start + 9 * M)
                .is_empty()
        );
        assert_eq!(fresh.evaluate(&[silent], start + 10 * M).len(), 1);

        // Went silent while the server was down: reported at the first check.
        let mut fresh = Watcher::new(&config("2m", None));
        let t = fresh.evaluate(&[row("vps", seen, None)], seen + 60 * M);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, Kind::Silent);
    }

    #[test]
    fn several_hosts_are_independent() {
        let mut w = Watcher::new(&config("1m", None));
        let now = 1_760_000_000_000;
        let rows = [
            row("a", now - 2 * M, None),
            HostRow {
                id: 2,
                ..row("b", now - 10 * S, None)
            },
            HostRow {
                id: 3,
                ..row("c", now - 5 * S, Some(now - M))
            },
        ];
        let t = w.evaluate(&rows, now);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].host.as_str(), &t[0].kind), ("a", &Kind::Silent));
        assert_eq!(
            (t[1].host.as_str(), &t[1].kind, t[1].host_id),
            ("c", &Kind::Reporting, 3)
        );
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_utc(1_760_000_000_000), "2025-10-09 08:53:20 UTC");
        assert_eq!(format_utc(1_791_693_631_151), "2026-10-11 04:40:31 UTC");
    }
}
