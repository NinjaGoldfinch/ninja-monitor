//! Alert rule evaluation. Each rule runs a small state machine:
//!
//! ```text
//! ok --(true)--> pending --(true for `for`)--> firing --(false)--> ok
//!                pending --(false)--> ok
//! ```
//!
//! Time comes from the caller as monotonic milliseconds, so tests can drive
//! it with a fake clock.

pub mod notify;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::warn;

use crate::config::{AlertRule, Config, Metric, Scope};
use crate::sample::{Sample, ServiceSample, ServiceState};
use crate::sampler::round2;
use notify::Queue;

const EVENT_LOG_LEN: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleState {
    Ok,
    Pending,
    Firing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Firing,
    Repeat,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlertEvent {
    /// Unix milliseconds of the sample that triggered the event.
    pub timestamp: u64,
    pub rule: String,
    pub scope: String,
    pub metric: &'static str,
    pub kind: EventKind,
    pub value: Option<f64>,
    pub op: Option<&'static str>,
    pub threshold: Option<f64>,
    /// How long the condition had held when the event was raised.
    pub held_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct RuleView {
    pub name: String,
    pub scope: String,
    pub metric: &'static str,
    pub op: Option<&'static str>,
    pub threshold: Option<f64>,
    pub for_secs: Option<u64>,
    pub state: RuleState,
    /// Unix milliseconds when the current pending or firing state began.
    pub since: Option<u64>,
    pub value: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct AlertsView {
    pub rules: Vec<RuleView>,
    /// Newest first.
    pub events: Vec<AlertEvent>,
}

struct Runtime {
    rule: AlertRule,
    state: RuleState,
    since_mono: u64,
    since_wall: u64,
    last_sent_mono: u64,
    last_value: Option<f64>,
    last_count: Option<u64>,
}

pub struct Engine {
    rules: Vec<Runtime>,
    events: VecDeque<AlertEvent>,
}

impl Engine {
    pub fn new(config: &Config) -> Self {
        Self {
            rules: config
                .alerts
                .iter()
                .map(|rule| Runtime {
                    rule: rule.clone(),
                    state: RuleState::Ok,
                    since_mono: 0,
                    since_wall: 0,
                    last_sent_mono: 0,
                    last_value: None,
                    last_count: None,
                })
                .collect(),
            events: VecDeque::with_capacity(EVENT_LOG_LEN),
        }
    }

    /// Evaluates every rule against `sample` at monotonic time `now_ms` and
    /// returns the events that should be sent.
    pub fn evaluate(&mut self, sample: &Sample, now_ms: u64) -> Vec<AlertEvent> {
        let mut out = Vec::new();
        for rt in &mut self.rules {
            if let Some(event) = step(rt, sample, now_ms) {
                out.push(event);
            }
        }
        for event in &out {
            if self.events.len() == EVENT_LOG_LEN {
                self.events.pop_front();
            }
            self.events.push_back(event.clone());
        }
        out
    }

    pub fn view(&self) -> AlertsView {
        AlertsView {
            rules: self
                .rules
                .iter()
                .map(|rt| RuleView {
                    name: rt.rule.name.clone(),
                    scope: rt.rule.scope.to_string(),
                    metric: rt.rule.metric.name(),
                    op: rt.rule.op.map(|o| o.symbol()),
                    threshold: rt.rule.threshold,
                    for_secs: rt.rule.for_duration.map(|d| d.as_secs()),
                    state: rt.state,
                    since: (rt.state != RuleState::Ok).then_some(rt.since_wall),
                    value: rt.last_value,
                })
                .collect(),
            events: self.events.iter().rev().cloned().collect(),
        }
    }
}

fn step(rt: &mut Runtime, sample: &Sample, now: u64) -> Option<AlertEvent> {
    let rule = &rt.rule;
    let service = match &rule.scope {
        Scope::Service(name) => sample.services.iter().find(|s| s.name == *name),
        Scope::Host => None,
    };

    if rule.metric == Metric::OomKill {
        let count = service.and_then(|s| s.oom_kills);
        rt.last_value = count.map(|c| c as f64);
        let count = count?;
        let previous = rt.last_count.replace(count);
        return match previous {
            Some(prev) if count > prev => Some(event(rt, sample, EventKind::Firing, 0)),
            _ => None,
        };
    }

    let (value, condition) = if rule.metric == Metric::Down {
        // Missing data counts as down.
        let down = service.is_none_or(|s| s.state == ServiceState::Down);
        (Some(if down { 1.0 } else { 0.0 }), Some(down))
    } else {
        let value = metric_value(rule, sample, service);
        let (op, threshold) = (rule.op?, rule.threshold?);
        (value, value.map(|v| op.holds(v, threshold)))
    };
    rt.last_value = value;
    // A missing value leaves the state alone.
    let condition = condition?;

    let for_ms = rule.for_duration.map_or(0, |d| d.as_millis() as u64);
    let held = |rt: &Runtime| now.saturating_sub(rt.since_mono) / 1000;
    match (rt.state, condition) {
        (RuleState::Ok, false) => None,
        (RuleState::Ok, true) => {
            rt.since_mono = now;
            rt.since_wall = sample.timestamp;
            if for_ms == 0 {
                fire(rt, sample, now, 0)
            } else {
                rt.state = RuleState::Pending;
                None
            }
        }
        (RuleState::Pending, true) => {
            if now.saturating_sub(rt.since_mono) >= for_ms {
                let held = held(rt);
                fire(rt, sample, now, held)
            } else {
                None
            }
        }
        (RuleState::Pending, false) => {
            rt.state = RuleState::Ok;
            None
        }
        (RuleState::Firing, true) => {
            let repeat = rt.rule.repeat?.as_millis() as u64;
            if now.saturating_sub(rt.last_sent_mono) >= repeat {
                rt.last_sent_mono = now;
                Some(event(rt, sample, EventKind::Repeat, held(rt)))
            } else {
                None
            }
        }
        (RuleState::Firing, false) => {
            rt.state = RuleState::Ok;
            Some(event(rt, sample, EventKind::Resolved, held(rt)))
        }
    }
}

fn fire(rt: &mut Runtime, sample: &Sample, now: u64, held_secs: u64) -> Option<AlertEvent> {
    rt.state = RuleState::Firing;
    rt.last_sent_mono = now;
    Some(event(rt, sample, EventKind::Firing, held_secs))
}

fn event(rt: &Runtime, sample: &Sample, kind: EventKind, held_secs: u64) -> AlertEvent {
    AlertEvent {
        timestamp: sample.timestamp,
        rule: rt.rule.name.clone(),
        scope: rt.rule.scope.to_string(),
        metric: rt.rule.metric.name(),
        kind,
        value: rt.last_value,
        op: rt.rule.op.map(|o| o.symbol()),
        threshold: rt.rule.threshold,
        held_secs,
    }
}

fn metric_value(rule: &AlertRule, sample: &Sample, service: Option<&ServiceSample>) -> Option<f64> {
    let h = &sample.host;
    let percent = |part: Option<u64>, whole: Option<u64>| {
        let whole = whole.filter(|&w| w > 0)?;
        Some(round2(part? as f64 / whole as f64 * 100.0))
    };
    let bytes = |v: Option<u64>| v.map(|b| b as f64);
    match (&rule.scope, rule.metric) {
        (Scope::Host, Metric::CpuPercent) => h.cpu_percent,
        (Scope::Host, Metric::Load1) => h.load1,
        (Scope::Host, Metric::MemAvailablePercent) => {
            percent(h.mem_available_bytes, h.mem_total_bytes)
        }
        (Scope::Host, Metric::MemAvailableBytes) => bytes(h.mem_available_bytes),
        // No swap at all means there is nothing to alert on.
        (Scope::Host, Metric::SwapUsedPercent) => percent(h.swap_used_bytes, h.swap_total_bytes),
        (Scope::Host, Metric::DiskUsedPercent) => {
            let mount = rule.mount.as_ref()?.display().to_string();
            h.disks.iter().find(|d| d.mount == mount)?.used_percent
        }
        (Scope::Service(_), metric) => {
            let s = service?;
            match metric {
                Metric::CpuPercent => s.cpu_percent,
                Metric::MemAnonBytes => bytes(s.mem_anon_bytes),
                Metric::MemCurrentBytes => bytes(s.mem_current_bytes),
                Metric::Pids => bytes(s.pids),
                Metric::DiskBytes => bytes(s.disk_bytes),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Bus subscriber: evaluates rules and queues notifications.
pub async fn run(
    engine: Arc<Mutex<Engine>>,
    mut rx: broadcast::Receiver<Arc<Sample>>,
    queue: Option<Arc<Queue>>,
    host_name: String,
) {
    let start = Instant::now();
    loop {
        let sample = match rx.recv().await {
            Ok(sample) => sample,
            Err(RecvError::Lagged(missed)) => {
                warn!(missed, "alert engine fell behind the sampler");
                continue;
            }
            Err(RecvError::Closed) => break,
        };
        let now = start.elapsed().as_millis() as u64;
        let events = engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .evaluate(&sample, now);
        for event in events {
            tracing::info!(rule = %event.rule, kind = ?event.kind, "alert");
            if let Some(queue) = &queue {
                queue.push(notify::format_event(&host_name, &event));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{DiskSample, HostSample};

    const MIB: u64 = 1024 * 1024;

    fn engine(rules: &str) -> Engine {
        let config = Config::parse(&format!(
            r#"
            [host]
            mounts = ["/"]
            [[service]]
            name = "app"
            unit = "app.service"
            {rules}
            "#
        ))
        .unwrap();
        Engine::new(&config)
    }

    /// A healthy host and service; tests tweak fields from here.
    fn sample() -> Sample {
        Sample {
            timestamp: 1_000_000,
            host: HostSample {
                name: "vps".into(),
                cpu_percent: Some(10.0),
                cores: Some(2),
                load1: Some(0.5),
                load5: Some(0.5),
                load15: Some(0.5),
                mem_total_bytes: Some(1000 * MIB),
                mem_available_bytes: Some(500 * MIB),
                mem_used_bytes: Some(500 * MIB),
                mem_cache_bytes: Some(100 * MIB),
                swap_total_bytes: Some(0),
                swap_used_bytes: Some(0),
                uptime_secs: Some(100),
                disks: vec![DiskSample {
                    mount: "/".into(),
                    total_bytes: Some(100),
                    used_bytes: Some(50),
                    avail_bytes: Some(50),
                    used_percent: Some(50.0),
                }],
            },
            services: vec![ServiceSample {
                name: "app".into(),
                unit: "app.service".into(),
                state: ServiceState::Running,
                cpu_percent: Some(5.0),
                mem_current_bytes: Some(300 * MIB),
                mem_anon_bytes: Some(100 * MIB),
                mem_file_bytes: Some(200 * MIB),
                mem_peak_bytes: None,
                oom_kills: Some(0),
                pids: Some(4),
                restarts_seen: 0,
                disk_bytes: None,
                disk_measured_at: None,
            }],
        }
    }

    fn with_anon(mib: Option<u64>) -> Sample {
        let mut s = sample();
        s.services[0].mem_anon_bytes = mib.map(|m| m * MIB);
        s
    }

    const MEM_RULE: &str = r#"
        [[alert]]
        name = "mem"
        scope = "service:app"
        metric = "mem_anon_bytes"
        op = ">"
        threshold = 209715200
        for = "5m"
    "#;

    fn state(e: &Engine) -> RuleState {
        e.view().rules[0].state
    }

    #[test]
    fn pending_then_firing_after_for() {
        let mut e = engine(MEM_RULE);
        assert!(e.evaluate(&with_anon(Some(100)), 0).is_empty());
        assert_eq!(state(&e), RuleState::Ok);

        assert!(e.evaluate(&with_anon(Some(300)), 1_000).is_empty());
        assert_eq!(state(&e), RuleState::Pending);
        assert!(e.evaluate(&with_anon(Some(300)), 200_000).is_empty());
        assert!(e.evaluate(&with_anon(Some(300)), 300_999).is_empty());

        let events = e.evaluate(&with_anon(Some(300)), 301_000);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.kind, EventKind::Firing);
        assert_eq!(ev.value, Some((300 * MIB) as f64));
        assert_eq!(ev.threshold, Some(209715200.0));
        assert_eq!(ev.op, Some(">"));
        assert_eq!(ev.held_secs, 300);
        assert_eq!(ev.scope, "service:app");
        assert_eq!(state(&e), RuleState::Firing);

        // Nothing more while it stays firing.
        assert!(e.evaluate(&with_anon(Some(400)), 900_000).is_empty());
    }

    #[test]
    fn flapping_below_for_never_fires() {
        let mut e = engine(MEM_RULE);
        for i in 0..20 {
            let mib = if i % 2 == 0 { 300 } else { 100 };
            let events = e.evaluate(&with_anon(Some(mib)), i * 200_000);
            assert!(events.is_empty(), "fired at step {i}");
        }
        assert_eq!(state(&e), RuleState::Ok);
    }

    #[test]
    fn resolves_once() {
        let mut e = engine(MEM_RULE);
        e.evaluate(&with_anon(Some(300)), 0);
        assert_eq!(e.evaluate(&with_anon(Some(300)), 300_000).len(), 1);
        let events = e.evaluate(&with_anon(Some(150)), 420_000);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Resolved);
        assert_eq!(events[0].held_secs, 420);
        assert_eq!(events[0].value, Some((150 * MIB) as f64));
        assert_eq!(state(&e), RuleState::Ok);
        assert!(e.evaluate(&with_anon(Some(150)), 500_000).is_empty());
    }

    #[test]
    fn repeat_while_firing() {
        let mut e = engine(&format!("{MEM_RULE}\nrepeat = \"6h\""));
        e.evaluate(&with_anon(Some(300)), 0);
        assert_eq!(
            e.evaluate(&with_anon(Some(300)), 300_000)[0].kind,
            EventKind::Firing
        );
        let six_h = 6 * 3_600_000;
        assert!(
            e.evaluate(&with_anon(Some(300)), 300_000 + six_h - 1)
                .is_empty()
        );
        let events = e.evaluate(&with_anon(Some(300)), 300_000 + six_h);
        assert_eq!(events[0].kind, EventKind::Repeat);
        assert!(
            e.evaluate(&with_anon(Some(300)), 300_000 + six_h + 1000)
                .is_empty()
        );
        let events = e.evaluate(&with_anon(Some(300)), 300_000 + 2 * six_h);
        assert_eq!(events[0].kind, EventKind::Repeat);
    }

    #[test]
    fn null_values_leave_state_unchanged() {
        let mut e = engine(MEM_RULE);
        e.evaluate(&with_anon(Some(300)), 0);
        assert!(e.evaluate(&with_anon(None), 100_000).is_empty());
        assert_eq!(state(&e), RuleState::Pending);
        // Pending time keeps counting across the gap.
        assert_eq!(e.evaluate(&with_anon(Some(300)), 300_000).len(), 1);
        // While firing, a gap neither resolves nor re-fires.
        assert!(e.evaluate(&with_anon(None), 400_000).is_empty());
        assert_eq!(state(&e), RuleState::Firing);
        // While ok, a gap does nothing either.
        e.evaluate(&with_anon(Some(100)), 500_000);
        assert!(e.evaluate(&with_anon(None), 600_000).is_empty());
        assert_eq!(state(&e), RuleState::Ok);
    }

    #[test]
    fn no_for_fires_on_first_match() {
        let mut e = engine(
            r#"
            [[alert]]
            name = "load"
            scope = "host"
            metric = "load1"
            op = ">="
            threshold = 4
            "#,
        );
        let mut s = sample();
        s.host.load1 = Some(4.0);
        let events = e.evaluate(&s, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].held_secs, 0);
    }

    #[test]
    fn down_counts_missing_data() {
        let mut e = engine(
            r#"
            [[alert]]
            name = "app down"
            scope = "service:app"
            metric = "down"
            for = "1m"
            "#,
        );
        assert!(e.evaluate(&sample(), 0).is_empty());

        let mut down = sample();
        down.services[0].state = ServiceState::Down;
        assert!(e.evaluate(&down, 1_000).is_empty());
        assert_eq!(state(&e), RuleState::Pending);

        // A service missing from the sample entirely is also down.
        let mut missing = sample();
        missing.services.clear();
        let events = e.evaluate(&missing, 61_000);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Firing);
        assert_eq!(events[0].op, None);

        let events = e.evaluate(&sample(), 70_000);
        assert_eq!(events[0].kind, EventKind::Resolved);
    }

    #[test]
    fn oom_kill_fires_on_each_increase_without_resolve() {
        let mut e = engine(
            r#"
            [[alert]]
            name = "oom"
            scope = "service:app"
            metric = "oom_kill"
            "#,
        );
        let at = |count: Option<u64>| {
            let mut s = sample();
            s.services[0].oom_kills = count;
            s
        };
        // The first reading is only a baseline, even if it is not zero.
        assert!(e.evaluate(&at(Some(2)), 0).is_empty());
        assert!(e.evaluate(&at(Some(2)), 1).is_empty());
        let events = e.evaluate(&at(Some(3)), 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Firing);
        assert_eq!(events[0].value, Some(3.0));
        assert_eq!(state(&e), RuleState::Ok);
        assert!(e.evaluate(&at(Some(3)), 3).is_empty());
        assert!(e.evaluate(&at(None), 4).is_empty());
        assert_eq!(e.evaluate(&at(Some(5)), 5).len(), 1);
        // A restart resets the counter; that alone is not an OOM kill.
        assert!(e.evaluate(&at(Some(0)), 6).is_empty());
        assert_eq!(e.evaluate(&at(Some(1)), 7).len(), 1);
    }

    #[test]
    fn host_metrics() {
        let rules = r#"
            [[alert]]
            name = "mem low"
            scope = "host"
            metric = "mem_available_percent"
            op = "<"
            threshold = 10
            [[alert]]
            name = "disk"
            scope = "host"
            metric = "disk_used_percent"
            mount = "/"
            op = ">"
            threshold = 85
            [[alert]]
            name = "swap"
            scope = "host"
            metric = "swap_used_percent"
            op = ">"
            threshold = 1
        "#;
        let mut e = engine(rules);
        assert!(e.evaluate(&sample(), 0).is_empty());
        let view = e.view();
        assert_eq!(view.rules[0].value, Some(50.0));
        assert_eq!(view.rules[1].value, Some(50.0));
        assert_eq!(view.rules[2].value, None, "no swap means no value");

        let mut s = sample();
        s.host.mem_available_bytes = Some(50 * MIB);
        s.host.disks[0].used_percent = Some(90.0);
        let events = e.evaluate(&s, 1);
        let names: Vec<_> = events.iter().map(|e| e.rule.as_str()).collect();
        assert_eq!(names, ["mem low", "disk"]);
        assert_eq!(events[0].value, Some(5.0));

        s.host.mem_available_bytes = Some(333 * MIB);
        e.evaluate(&s, 2);
        assert_eq!(e.view().rules[0].value, Some(33.3));
    }

    #[test]
    fn event_log_is_bounded_and_newest_first() {
        let mut e = engine(
            r#"
            [[alert]]
            name = "cpu"
            scope = "service:app"
            metric = "cpu_percent"
            op = ">"
            threshold = 50
            "#,
        );
        for i in 0..150u64 {
            let mut s = sample();
            s.timestamp = i;
            s.services[0].cpu_percent = Some(if i % 2 == 0 { 90.0 } else { 10.0 });
            assert_eq!(e.evaluate(&s, i).len(), 1);
        }
        let view = e.view();
        assert_eq!(view.events.len(), 100);
        assert_eq!(view.events[0].timestamp, 149);
        assert_eq!(view.events[0].kind, EventKind::Resolved);
        assert_eq!(view.events[99].timestamp, 50);
    }

    #[test]
    fn view_reports_since_and_settings() {
        let mut e = engine(MEM_RULE);
        let mut s = with_anon(Some(300));
        s.timestamp = 42;
        e.evaluate(&s, 0);
        let view = e.view();
        let rule = &view.rules[0];
        assert_eq!(rule.state, RuleState::Pending);
        assert_eq!(rule.since, Some(42));
        assert_eq!(rule.for_secs, Some(300));
        assert_eq!(rule.op, Some(">"));
        assert_eq!(rule.metric, "mem_anon_bytes");
        e.evaluate(&with_anon(Some(1)), 1);
        assert_eq!(e.view().rules[0].since, None);
    }
}
