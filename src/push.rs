//! Pushes samples and alert events to a ninja-ingest server.
//!
//! `State` holds the buffer, the back-off and the "ingest unreachable" alert
//! and is driven with explicit times so tests can use a fake clock. `run`
//! wraps it in the bus subscriber that does the actual HTTP.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::alerts::AlertEvent;
use crate::alerts::notify::{Queue, describe_error, human_duration};
use crate::config::PushConfig;
use crate::sample::Sample;

/// Most samples sent in one request; the server refuses more.
pub const MAX_BATCH: usize = 200;
/// Alert events kept while the server is unreachable.
pub const EVENT_CAPACITY: usize = 100;
/// Longest wait between attempts while the server keeps failing.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Time allowed for the last flush at shutdown.
pub const SHUTDOWN_FLUSH: Duration = Duration::from_secs(3);
const UNAUTHORIZED_LOG_INTERVAL: u64 = 60_000;

/// The request body, serialised straight from the buffered samples. Going
/// through a `serde_json::Value` tree first would cost several KiB per sample.
#[derive(Serialize)]
struct Body<'a> {
    agent: &'static str,
    host: &'a str,
    samples: Vec<&'a Sample>,
    events: Vec<&'a AlertEvent>,
}

/// What one POST came back with, reduced to what the state machine needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 2xx: the server stored (or already had) everything sent.
    Accepted,
    /// 400: the server refused the batch and retrying cannot help.
    Rejected(String),
    /// 401: wrong token. Keep buffering, say so once a minute.
    Unauthorized,
    /// 413: send smaller batches.
    TooLarge,
    /// 429, 5xx, timeout or connection failure. Back off and retry.
    Failed(String),
}

/// What was sent in one request, so success removes exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub samples: usize,
    pub events: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    Ok,
    Firing { since_ms: u64 },
}

pub struct State {
    host: String,
    samples: VecDeque<Arc<Sample>>,
    events: VecDeque<AlertEvent>,
    capacity: usize,
    /// Set while samples are being dropped, so the warning is logged once.
    overflowing: bool,
    dropped: u64,
    flush_ms: u64,
    alert_after_ms: u64,
    max_batch: usize,
    failures: u32,
    /// Monotonic time of the next attempt allowed.
    next_attempt_ms: u64,
    last_success_ms: Option<u64>,
    last_unauthorized_log_ms: Option<u64>,
    /// Samples sent since the server was last unreachable, for the log line.
    caught_up: usize,
    reachable: Option<bool>,
    alert: Alert,
}

impl State {
    pub fn new(host: &str, config: &PushConfig, sampling_interval: Duration) -> Self {
        let flush = config.flush_interval(sampling_interval);
        Self {
            host: host.to_string(),
            samples: VecDeque::new(),
            events: VecDeque::new(),
            capacity: config.capacity(sampling_interval),
            overflowing: false,
            dropped: 0,
            flush_ms: flush.as_millis().max(1) as u64,
            alert_after_ms: config.alert_after.as_millis() as u64,
            max_batch: MAX_BATCH,
            failures: 0,
            next_attempt_ms: 0,
            last_success_ms: None,
            last_unauthorized_log_ms: None,
            caught_up: 0,
            reachable: None,
            alert: Alert::Ok,
        }
    }

    pub fn buffered(&self) -> usize {
        self.samples.len()
    }

    pub fn pending_events(&self) -> usize {
        self.events.len()
    }

    pub fn alert(&self) -> Alert {
        self.alert
    }

    pub fn push_sample(&mut self, sample: Arc<Sample>) {
        if self.samples.len() >= self.capacity {
            self.samples.pop_front();
            self.dropped += 1;
            if !self.overflowing {
                self.overflowing = true;
                warn!(
                    capacity = self.capacity,
                    "push buffer full, dropping the oldest samples until the server is reachable"
                );
            }
        }
        self.samples.push_back(sample);
    }

    pub fn push_event(&mut self, event: AlertEvent) {
        if self.events.len() >= EVENT_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Whether an attempt should be made now.
    pub fn due(&self, now_ms: u64) -> bool {
        (!self.samples.is_empty() || !self.events.is_empty()) && now_ms >= self.next_attempt_ms
    }

    /// The next request body and what it contains, or `None` if the buffer
    /// is empty.
    pub fn next_batch(&self) -> Option<(Batch, String)> {
        if self.samples.is_empty() && self.events.is_empty() {
            return None;
        }
        let samples: Vec<&Sample> = self
            .samples
            .iter()
            .take(self.max_batch)
            .map(|s| s.as_ref())
            .collect();
        let events: Vec<&AlertEvent> = self.events.iter().collect();
        let batch = Batch {
            samples: samples.len(),
            events: events.len(),
        };
        let body = serde_json::to_string(&Body {
            agent: concat!("ninja-monitor/", env!("CARGO_PKG_VERSION")),
            host: &self.host,
            samples,
            events,
        })
        .unwrap_or_default();
        Some((batch, body))
    }

    /// Applies the result of sending `batch`. Returns a Discord message when
    /// the unreachable alert fires or resolves.
    pub fn settle(&mut self, batch: &Batch, outcome: &Outcome, now_ms: u64) -> Option<String> {
        match outcome {
            Outcome::Accepted => {
                self.remove(batch);
                self.failures = 0;
                self.next_attempt_ms = now_ms + self.flush_ms;
                self.max_batch = MAX_BATCH;
                self.overflowing = false;
                self.caught_up += batch.samples;
                let was_down = self.reachable == Some(false);
                if self.reachable != Some(true) {
                    if was_down {
                        info!(
                            caught_up = self.caught_up,
                            dropped = self.dropped,
                            "ingest server reachable again"
                        );
                    } else {
                        info!("pushing to the ingest server");
                    }
                    self.reachable = Some(true);
                }
                self.dropped = 0;
                let message = match self.alert {
                    Alert::Firing { since_ms } => {
                        self.alert = Alert::Ok;
                        Some(format!(
                            "**RESOLVED** [{}] ingest unreachable: reachable again after {}, {} samples caught up",
                            self.host,
                            human_duration((now_ms - since_ms) / 1000),
                            self.caught_up
                        ))
                    }
                    Alert::Ok => None,
                };
                self.caught_up = 0;
                self.last_success_ms = Some(now_ms);
                message
            }
            Outcome::Rejected(reason) => {
                warn!(
                    samples = batch.samples,
                    events = batch.events,
                    %reason,
                    "ingest server rejected a batch; dropping it"
                );
                self.remove(batch);
                self.next_attempt_ms = now_ms + self.flush_ms;
                None
            }
            Outcome::TooLarge => {
                self.max_batch = (self.max_batch / 2).max(1);
                debug!(
                    max_batch = self.max_batch,
                    "ingest server said the batch was too large"
                );
                self.next_attempt_ms = now_ms + self.flush_ms;
                None
            }
            Outcome::Unauthorized => {
                if self
                    .last_unauthorized_log_ms
                    .is_none_or(|t| now_ms - t >= UNAUTHORIZED_LOG_INTERVAL)
                {
                    warn!(
                        "ingest server rejected the push token; check NINJA_MONITOR_PUSH_TOKEN on both sides"
                    );
                    self.last_unauthorized_log_ms = Some(now_ms);
                }
                self.fail(now_ms)
            }
            Outcome::Failed(reason) => {
                if self.reachable != Some(false) {
                    info!(%reason, buffered = self.samples.len(), "ingest server unreachable, buffering");
                    self.reachable = Some(false);
                } else {
                    debug!(%reason, buffered = self.samples.len(), "push failed");
                }
                self.fail(now_ms)
            }
        }
    }

    fn fail(&mut self, now_ms: u64) -> Option<String> {
        self.failures = self.failures.saturating_add(1);
        self.next_attempt_ms = now_ms + self.backoff_ms();
        self.check_alert(now_ms)
    }

    /// 1, 2 then 4 flush intervals, never more than `MAX_BACKOFF`. With the
    /// default 15 second interval that is 15s, 30s, 60s, 60s...
    fn backoff_ms(&self) -> u64 {
        let factor = 1u64 << (self.failures.saturating_sub(1)).min(2);
        (self.flush_ms * factor).min(MAX_BACKOFF.as_millis() as u64)
    }

    /// Fires the unreachable alert once no POST has succeeded for
    /// `alert_after`. Call after each failure and on a timer, so a quiet
    /// buffer still raises it.
    pub fn check_alert(&mut self, now_ms: u64) -> Option<String> {
        if self.alert_after_ms == 0 || self.alert != Alert::Ok || self.reachable != Some(false) {
            return None;
        }
        let since = self.last_success_ms.unwrap_or(0);
        if now_ms.saturating_sub(since) < self.alert_after_ms {
            return None;
        }
        self.alert = Alert::Firing { since_ms: since };
        Some(format!(
            "**FIRING** [{}] ingest unreachable: no successful push for {} ({} samples buffered)",
            self.host,
            human_duration((now_ms - since) / 1000),
            self.samples.len()
        ))
    }

    fn remove(&mut self, batch: &Batch) {
        self.samples.drain(..batch.samples.min(self.samples.len()));
        self.events.drain(..batch.events.min(self.events.len()));
    }
}

/// One POST, on the blocking pool. Any 2xx counts as accepted.
pub fn post(agent: &ureq::Agent, url: &str, token: &str, body: &str) -> Outcome {
    let response = agent
        .post(url)
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(body);
    let mut response = match response {
        Ok(response) => response,
        Err(err) => return Outcome::Failed(describe_error(&err)),
    };
    let status = response.status().as_u16();
    match status {
        200..=299 => Outcome::Accepted,
        400 => Outcome::Rejected(
            response
                .body_mut()
                .read_to_string()
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| "HTTP 400".to_string()),
        ),
        401 => Outcome::Unauthorized,
        413 => Outcome::TooLarge,
        _ => Outcome::Failed(format!("HTTP {status}")),
    }
}

pub struct Pusher {
    pub state: State,
    pub agent: ureq::Agent,
    pub url: String,
    pub token: String,
    pub queue: Option<Arc<Queue>>,
}

impl Pusher {
    fn spawn_post(&self, body: String) -> JoinHandle<Outcome> {
        let (agent, url, token) = (self.agent.clone(), self.url.clone(), self.token.clone());
        tokio::task::spawn_blocking(move || post(&agent, &url, &token, &body))
    }

    fn notify(&self, message: Option<String>) {
        if let Some(message) = message {
            match &self.queue {
                Some(queue) => queue.push(message),
                None => warn!("{message}"),
            }
        }
    }
}

/// Bus subscriber. Runs until `shutdown` turns true, then makes one last
/// attempt bounded by `SHUTDOWN_FLUSH`.
pub async fn run(
    mut pusher: Pusher,
    mut samples: broadcast::Receiver<Arc<Sample>>,
    mut events: broadcast::Receiver<AlertEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    let start = Instant::now();
    let now_ms = |start: Instant| start.elapsed().as_millis() as u64;
    let mut ticker = tokio::time::interval(Duration::from_millis(pusher.state.flush_ms.min(1000)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut inflight: Option<(Batch, JoinHandle<Outcome>)> = None;

    loop {
        tokio::select! {
            received = samples.recv() => match received {
                Ok(sample) => pusher.state.push_sample(sample),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "pusher fell behind the sampler")
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            received = events.recv() => match received {
                Ok(event) => pusher.state.push_event(event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "pusher missed alert events")
                }
                Err(broadcast::error::RecvError::Closed) => {}
            },
            result = async { (&mut inflight.as_mut().unwrap().1).await }, if inflight.is_some() => {
                let (batch, _) = inflight.take().unwrap();
                let outcome = result.unwrap_or_else(|e| Outcome::Failed(format!("task failed: {e}")));
                let message = pusher.state.settle(&batch, &outcome, now_ms(start));
                pusher.notify(message);
            }
            _ = ticker.tick() => {
                let now = now_ms(start);
                let message = pusher.state.check_alert(now);
                pusher.notify(message);
                if inflight.is_none()
                    && pusher.state.due(now)
                    && let Some((batch, body)) = pusher.state.next_batch()
                {
                    inflight = Some((batch, pusher.spawn_post(body)));
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // Last chance for whatever is buffered. Only when the server was
    // reachable; a dead server would just eat the whole timeout.
    if let Some((batch, handle)) = inflight.take() {
        if let Ok(Ok(outcome)) = tokio::time::timeout(SHUTDOWN_FLUSH, handle).await {
            pusher.state.settle(&batch, &outcome, now_ms(start));
        }
    } else if pusher.state.reachable == Some(true)
        && let Some((batch, body)) = pusher.state.next_batch()
    {
        debug!(
            samples = batch.samples,
            events = batch.events,
            "final push before shutdown"
        );
        if let Ok(Ok(outcome)) = tokio::time::timeout(SHUTDOWN_FLUSH, pusher.spawn_post(body)).await
        {
            pusher.state.settle(&batch, &outcome, now_ms(start));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::EventKind;
    use crate::config::Config;
    use crate::sample::HostSample;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            r#"
            [sampling]
            interval = "15s"
            [push]
            enabled = true
            url = "https://ingest.example/v1/ingest"
            {extra}
            "#
        ))
        .unwrap()
    }

    fn state(extra: &str) -> State {
        let config = config(extra);
        State::new("vps", &config.push, config.sampling.interval)
    }

    fn sample(t: u64) -> Arc<Sample> {
        Arc::new(Sample {
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
        })
    }

    fn event(t: u64) -> AlertEvent {
        AlertEvent {
            timestamp: t,
            rule: "r".into(),
            scope: "host".into(),
            metric: "load1".into(),
            kind: EventKind::Firing,
            value: None,
            op: None,
            threshold: None,
            held_secs: 0,
        }
    }

    const S: u64 = 1000;

    #[test]
    fn defaults_follow_the_sampling_interval() {
        let s = state("");
        assert_eq!(s.flush_ms, 15 * S);
        assert_eq!(s.capacity, 240);
        assert_eq!(s.alert_after_ms, 10 * 60 * S);
        assert!(!s.due(0));
        assert!(s.next_batch().is_none());
    }

    #[test]
    fn buffer_drops_oldest_when_full() {
        let mut s = state("buffer = \"1m\"");
        assert_eq!(s.capacity, 4);
        for t in 1..=6 {
            s.push_sample(sample(t));
        }
        assert_eq!(s.buffered(), 4);
        assert!(s.overflowing);
        assert_eq!(s.dropped, 2);
        let (batch, body) = s.next_batch().unwrap();
        assert_eq!(
            batch,
            Batch {
                samples: 4,
                events: 0
            }
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["host"], "vps");
        assert_eq!(v["samples"][0]["timestamp"], 3);
        assert!(v["agent"].as_str().unwrap().starts_with("ninja-monitor/"));
    }

    #[test]
    fn batch_is_capped_and_success_removes_only_what_was_sent() {
        let mut s = state("buffer = \"2h\"");
        for t in 0..250 {
            s.push_sample(sample(t));
        }
        s.push_event(event(1));
        let (batch, _) = s.next_batch().unwrap();
        assert_eq!(
            batch,
            Batch {
                samples: 200,
                events: 1
            }
        );
        // A new sample and event arrive while the request is in flight.
        s.push_sample(sample(250));
        s.push_event(event(2));
        assert!(s.settle(&batch, &Outcome::Accepted, 100 * S).is_none());
        assert_eq!(s.buffered(), 51);
        assert_eq!(s.pending_events(), 1);
        assert_eq!(s.next_batch().unwrap().0.events, 1);
        assert!(!s.due(100 * S + 14 * S));
        assert!(s.due(100 * S + 15 * S));
    }

    #[test]
    fn backoff_is_one_two_four_intervals_capped() {
        let mut s = state("");
        s.push_sample(sample(1));
        let (batch, _) = s.next_batch().unwrap();
        let mut now = 0;
        let mut waits = Vec::new();
        for _ in 0..5 {
            s.settle(&batch, &Outcome::Failed("x".into()), now);
            waits.push(s.next_attempt_ms - now);
            now = s.next_attempt_ms;
        }
        assert_eq!(waits, [15 * S, 30 * S, 60 * S, 60 * S, 60 * S]);
        assert_eq!(s.buffered(), 1, "failures keep the sample");
        s.settle(&batch, &Outcome::Accepted, now);
        assert_eq!(s.failures, 0);
        assert_eq!(s.next_attempt_ms, now + 15 * S);

        // Short intervals back off proportionally but never past 60s.
        let mut quick = State::new(
            "vps",
            &config("flush_interval = \"100ms\"\nbuffer = \"10s\"").push,
            Duration::from_secs(15),
        );
        quick.push_sample(sample(1));
        let (batch, _) = quick.next_batch().unwrap();
        quick.settle(&batch, &Outcome::Failed("x".into()), 0);
        assert_eq!(quick.next_attempt_ms, 100);
        quick.settle(&batch, &Outcome::Failed("x".into()), 100);
        assert_eq!(quick.next_attempt_ms, 300);
        quick.settle(&batch, &Outcome::Failed("x".into()), 300);
        assert_eq!(quick.next_attempt_ms, 700);
    }

    #[test]
    fn rejected_drops_the_batch_and_too_large_halves_it() {
        let mut s = state("");
        for t in 0..3 {
            s.push_sample(sample(t));
        }
        let (batch, _) = s.next_batch().unwrap();
        s.settle(&batch, &Outcome::TooLarge, 0);
        assert_eq!(s.max_batch, 100);
        assert_eq!(s.buffered(), 3);
        s.settle(&batch, &Outcome::Rejected("bad".into()), 0);
        assert_eq!(s.buffered(), 0);
        s.push_sample(sample(9));
        let (batch, _) = s.next_batch().unwrap();
        s.settle(&batch, &Outcome::Accepted, 0);
        assert_eq!(s.max_batch, MAX_BATCH);
    }

    #[test]
    fn unauthorized_keeps_buffering_and_backs_off() {
        let mut s = state("");
        s.push_sample(sample(1));
        let (batch, _) = s.next_batch().unwrap();
        s.settle(&batch, &Outcome::Unauthorized, 0);
        assert_eq!(s.buffered(), 1);
        assert_eq!(s.last_unauthorized_log_ms, Some(0));
        s.settle(&batch, &Outcome::Unauthorized, 30 * S);
        assert_eq!(
            s.last_unauthorized_log_ms,
            Some(0),
            "logged at most once a minute"
        );
        s.settle(&batch, &Outcome::Unauthorized, 61 * S);
        assert_eq!(s.last_unauthorized_log_ms, Some(61 * S));
    }

    #[test]
    fn unreachable_alert_fires_and_resolves() {
        let mut s = state("alert_after = \"1m\"");
        s.push_sample(sample(1));
        let (batch, _) = s.next_batch().unwrap();
        // Successful first, so the alert measures from the last success.
        assert!(s.settle(&batch, &Outcome::Accepted, 0).is_none());
        s.push_sample(sample(2));
        let (batch, _) = s.next_batch().unwrap();
        assert!(
            s.settle(&batch, &Outcome::Failed("x".into()), 15 * S)
                .is_none()
        );
        assert!(
            s.settle(&batch, &Outcome::Failed("x".into()), 45 * S)
                .is_none()
        );
        assert_eq!(s.alert(), Alert::Ok);
        // The timer path catches it even with no attempt at that moment.
        let firing = s.check_alert(65 * S).unwrap();
        assert_eq!(
            firing,
            "**FIRING** [vps] ingest unreachable: no successful push for 1m 5s (1 samples buffered)"
        );
        assert_eq!(s.alert(), Alert::Firing { since_ms: 0 });
        assert!(s.check_alert(70 * S).is_none(), "fires once");
        assert!(
            s.settle(&batch, &Outcome::Failed("x".into()), 75 * S)
                .is_none()
        );

        s.push_sample(sample(3));
        let (batch, _) = s.next_batch().unwrap();
        let resolved = s.settle(&batch, &Outcome::Accepted, 120 * S).unwrap();
        assert_eq!(
            resolved,
            "**RESOLVED** [vps] ingest unreachable: reachable again after 2m, 2 samples caught up"
        );
        assert_eq!(s.alert(), Alert::Ok);
        assert!(
            s.check_alert(500 * S).is_none(),
            "reachable, nothing to say"
        );
    }

    #[test]
    fn alert_from_start_when_never_reachable_and_disabled_at_zero() {
        let mut s = state("alert_after = \"1m\"");
        s.push_sample(sample(1));
        let (batch, _) = s.next_batch().unwrap();
        assert!(
            s.settle(&batch, &Outcome::Failed("x".into()), 10 * S)
                .is_none()
        );
        assert!(s.check_alert(59 * S).is_none());
        assert!(s.check_alert(60 * S).is_some());

        let mut off = state("alert_after = \"0s\"");
        off.push_sample(sample(1));
        let (batch, _) = off.next_batch().unwrap();
        off.settle(&batch, &Outcome::Failed("x".into()), 0);
        assert!(off.check_alert(u64::MAX / 2).is_none());
    }

    #[test]
    fn events_are_bounded() {
        let mut s = state("");
        for t in 0..(EVENT_CAPACITY as u64 + 5) {
            s.push_event(event(t));
        }
        assert_eq!(s.pending_events(), EVENT_CAPACITY);
        assert!(s.due(0), "events alone are worth a request");
        assert_eq!(
            s.next_batch().unwrap().0,
            Batch {
                samples: 0,
                events: EVENT_CAPACITY
            }
        );
    }
}
