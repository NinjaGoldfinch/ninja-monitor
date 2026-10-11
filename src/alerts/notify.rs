//! Discord delivery: a bounded queue drained by its own task, so a slow or
//! failing webhook never holds up sampling or rule evaluation.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;
use tracing::{debug, warn};

use super::{AlertEvent, EventKind};

pub const QUEUE_CAPACITY: usize = 100;
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Bounded FIFO that drops the oldest message when full.
pub struct Queue {
    items: Mutex<VecDeque<String>>,
    capacity: usize,
    ready: Notify,
}

impl Queue {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            ready: Notify::new(),
        }
    }

    pub fn push(&self, message: String) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if items.len() == self.capacity {
            items.pop_front();
            warn!("notification queue full, dropped the oldest message");
        }
        items.push_back(message);
        drop(items);
        self.ready.notify_one();
    }

    pub async fn pop(&self) -> String {
        loop {
            if let Some(message) = self
                .items
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
            {
                return message;
            }
            self.ready.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared HTTP client for Discord and the heartbeat. Status codes are
/// returned as responses so 429 bodies can be read.
pub fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!("ninja-monitor/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Describes a request failure without echoing anything that could contain
/// the secret URL.
pub fn describe_error(err: &ureq::Error) -> String {
    match err {
        ureq::Error::Io(e) => format!("I/O error ({})", e.kind()),
        ureq::Error::Timeout(_) => "timed out".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "TLS error".to_string(),
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        _ => "request failed".to_string(),
    }
}

pub struct Discord {
    agent: ureq::Agent,
    url: String,
    backoff: Duration,
}

#[derive(Debug, PartialEq)]
enum Attempt {
    Delivered,
    RetryAfter(Duration),
    Retry(String),
    GiveUp(String),
}

impl Discord {
    pub fn new(agent: ureq::Agent, url: String) -> Self {
        Self::with_backoff(agent, url, Duration::from_secs(1))
    }

    /// `backoff` is the first retry delay; it doubles each retry.
    pub fn with_backoff(agent: ureq::Agent, url: String, backoff: Duration) -> Self {
        Self {
            agent,
            url,
            backoff,
        }
    }

    /// Posts one message, retrying up to three times. Returns whether it was delivered.
    pub async fn deliver(&self, content: &str) -> bool {
        // Never let a rule or host name ping @everyone.
        let body = json!({ "content": content, "allowed_mentions": { "parse": [] } }).to_string();
        let mut delay = self.backoff;
        for attempt in 0..=MAX_RETRIES {
            let (agent, url, body) = (self.agent.clone(), self.url.clone(), body.clone());
            let result = tokio::task::spawn_blocking(move || post(&agent, &url, &body))
                .await
                .unwrap_or_else(|e| Attempt::Retry(format!("task failed: {e}")));
            let wait = match result {
                Attempt::Delivered => return true,
                Attempt::GiveUp(reason) => {
                    warn!(%reason, "Discord rejected the notification, not retrying");
                    return false;
                }
                Attempt::RetryAfter(wait) => {
                    debug!(?wait, "Discord rate limited the notification");
                    wait
                }
                Attempt::Retry(reason) => {
                    warn!(%reason, attempt = attempt + 1, "Discord notification failed");
                    let wait = delay;
                    delay *= 2;
                    wait
                }
            };
            if attempt < MAX_RETRIES {
                tokio::time::sleep(wait).await;
            }
        }
        warn!("giving up on a Discord notification after {MAX_RETRIES} retries");
        false
    }
}

fn post(agent: &ureq::Agent, url: &str, body: &str) -> Attempt {
    let mut response = match agent
        .post(url)
        .header("content-type", "application/json")
        .send(body)
    {
        Ok(response) => response,
        Err(err) => return Attempt::Retry(describe_error(&err)),
    };
    let status = response.status().as_u16();
    match status {
        200..=299 => Attempt::Delivered,
        429 => {
            let header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok());
            let body = response
                .body_mut()
                .read_to_string()
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v["retry_after"].as_f64());
            let secs = body.or(header).unwrap_or(1.0);
            Attempt::RetryAfter(retry_after(secs))
        }
        500..=599 => Attempt::Retry(format!("HTTP {status}")),
        _ => Attempt::GiveUp(format!("HTTP {status}")),
    }
}

fn retry_after(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs).min(MAX_RETRY_AFTER)
    } else {
        Duration::from_secs(1)
    }
}

/// Drains the queue forever, one message at a time.
pub async fn run(queue: std::sync::Arc<Queue>, discord: Discord) {
    loop {
        let message = queue.pop().await;
        discord.deliver(&message).await;
    }
}

pub fn format_event(host: &str, e: &AlertEvent) -> String {
    let label = match e.kind {
        EventKind::Firing => "FIRING",
        EventKind::Repeat => "STILL FIRING",
        EventKind::Resolved => "RESOLVED",
    };
    let detail = match e.metric.as_str() {
        "down" => match e.kind {
            EventKind::Resolved => "service is running again".to_string(),
            _ => "service is down".to_string(),
        },
        "oom_kill" => format!(
            "the kernel OOM-killed a process (count now {})",
            e.value
                .map_or("?".to_string(), |v| format_value("oom_kill", v))
        ),
        metric => {
            let value = e
                .value
                .map_or("n/a".to_string(), |v| format_value(metric, v));
            match (e.op.as_deref(), e.threshold) {
                (Some(op), Some(t)) => {
                    format!(
                        "{metric} = {value} (threshold {op} {})",
                        format_value(metric, t)
                    )
                }
                _ => format!("{metric} = {value}"),
            }
        }
    };
    let held = match (e.kind, e.held_secs) {
        (_, 0) => String::new(),
        (EventKind::Resolved, secs) => format!(", lasted {}", human_duration(secs)),
        (_, secs) => format!(", for {}", human_duration(secs)),
    };
    format!("**{label}** [{host}] {}: {detail}{held}", e.rule)
}

pub fn format_value(metric: &str, value: f64) -> String {
    if metric.ends_with("_bytes") {
        human_bytes(value.max(0.0) as u64)
    } else if metric.ends_with("_percent") {
        format!("{}%", trim_number(value))
    } else {
        trim_number(value)
    }
}

fn trim_number(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    text.to_string()
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) if s == 0 => format!("{m}m"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(metric: &str, kind: EventKind, value: Option<f64>) -> AlertEvent {
        AlertEvent {
            timestamp: 0,
            rule: "riot-proxy memory high".into(),
            scope: "service:riot-proxy".into(),
            metric: metric.into(),
            kind,
            value,
            op: Some(">".into()),
            threshold: Some(2_000_000_000.0),
            held_secs: 312,
        }
    }

    #[test]
    fn byte_and_duration_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(2_000_000_000), "1.9 GiB");
        assert_eq!(
            human_bytes(5 * 1024 * 1024 * 1024 * 1024 * 1024),
            "5120.0 TiB"
        );
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(300), "5m");
        assert_eq!(human_duration(312), "5m 12s");
        assert_eq!(human_duration(3_780), "1h 3m");
        assert_eq!(human_duration(90_000), "1d 1h");
    }

    #[test]
    fn values() {
        assert_eq!(
            format_value("mem_anon_bytes", 3.0 * 1024.0 * 1024.0),
            "3.0 MiB"
        );
        assert_eq!(format_value("disk_used_percent", 85.0), "85%");
        assert_eq!(format_value("cpu_percent", 12.345), "12.35%");
        assert_eq!(format_value("load1", 1.5), "1.5");
        assert_eq!(format_value("pids", 12.0), "12");
    }

    #[test]
    fn messages() {
        let firing = format_event(
            "vps",
            &event("mem_anon_bytes", EventKind::Firing, Some(2_200_000_000.0)),
        );
        assert_eq!(
            firing,
            "**FIRING** [vps] riot-proxy memory high: mem_anon_bytes = 2.0 GiB (threshold > 1.9 GiB), for 5m 12s"
        );
        let resolved = format_event(
            "vps",
            &event("mem_anon_bytes", EventKind::Resolved, Some(1_000_000_000.0)),
        );
        assert!(resolved.starts_with("**RESOLVED** [vps]"), "{resolved}");
        assert!(resolved.ends_with(", lasted 5m 12s"), "{resolved}");

        let mut down = event("down", EventKind::Firing, Some(1.0));
        down.op = None;
        down.threshold = None;
        down.rule = "riot-proxy down".into();
        assert_eq!(
            format_event("vps", &down),
            "**FIRING** [vps] riot-proxy down: service is down, for 5m 12s"
        );

        let mut oom = event("oom_kill", EventKind::Firing, Some(3.0));
        oom.held_secs = 0;
        oom.rule = "oom".into();
        assert_eq!(
            format_event("vps", &oom),
            "**FIRING** [vps] oom: the kernel OOM-killed a process (count now 3)"
        );

        let repeat = format_event("vps", &event("cpu_percent", EventKind::Repeat, None));
        assert!(repeat.starts_with("**STILL FIRING**"), "{repeat}");
        assert!(repeat.contains("cpu_percent = n/a"), "{repeat}");
    }

    #[test]
    fn retry_after_bounds() {
        assert_eq!(retry_after(0.25), Duration::from_millis(250));
        assert_eq!(retry_after(-1.0), Duration::from_secs(1));
        assert_eq!(retry_after(f64::NAN), Duration::from_secs(1));
        assert_eq!(retry_after(1e9), MAX_RETRY_AFTER);
    }

    #[tokio::test]
    async fn queue_drops_oldest_when_full() {
        let queue = Queue::new(3);
        for i in 0..5 {
            queue.push(format!("m{i}"));
        }
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pop().await, "m2");
        assert_eq!(queue.pop().await, "m3");
        assert_eq!(queue.pop().await, "m4");
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn queue_wakes_a_waiting_consumer() {
        let queue = std::sync::Arc::new(Queue::new(10));
        let q = queue.clone();
        let consumer = tokio::spawn(async move { q.pop().await });
        tokio::task::yield_now().await;
        queue.push("hello".into());
        assert_eq!(consumer.await.unwrap(), "hello");
    }

    #[test]
    fn errors_never_include_the_url() {
        let agent = http_agent();
        let secret = "http://127.0.0.1:1/api/webhooks/123/very-secret-token";
        let err = agent.post(secret).send("{}").unwrap_err();
        let text = describe_error(&err);
        assert!(
            !text.contains("secret") && !text.contains("webhooks"),
            "{text}"
        );
        let bad = agent
            .post("not a url/very-secret-token")
            .send("{}")
            .unwrap_err();
        assert!(!describe_error(&bad).contains("secret"));
    }
}
