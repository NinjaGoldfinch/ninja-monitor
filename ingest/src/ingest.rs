//! The `POST /v1/ingest` protocol: request body, authentication and the
//! validation rules. Pure functions, so the rules are tested without HTTP.

use serde::Deserialize;

use ninja_monitor::alerts::AlertEvent;
use ninja_monitor::sample::Sample;

use crate::config::{Secrets, check_name};
use crate::token::constant_time_eq;

/// Most samples and most events accepted in one request.
pub const MAX_BATCH: usize = 200;
/// How far ahead of the server clock a sample may be dated.
pub const MAX_FUTURE_MS: u64 = 5 * 60_000;
/// Longest free-text field in an alert event.
const MAX_TEXT_LEN: usize = 256;

#[derive(Debug, Deserialize)]
pub struct IngestBody {
    /// The pushing agent's version string, such as `ninja-monitor/0.2.0`.
    #[serde(default)]
    pub agent: Option<String>,
    pub host: String,
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub events: Vec<AlertEvent>,
}

/// Why a request was refused, mapped to 413 or 400 by the API.
#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    TooLarge(String),
    Bad(String),
}

/// A request that passed validation. `rejected` counts samples dropped for
/// being outside the accepted time window.
#[derive(Debug)]
pub struct Checked {
    pub agent: Option<String>,
    pub samples: Vec<Sample>,
    pub events: Vec<AlertEvent>,
    pub rejected: usize,
}

/// Maps a bearer token to the agent it belongs to. Every stored token is
/// compared, in constant time, so the response time does not depend on
/// which token matched.
pub fn authenticate(header: Option<&str>, secrets: &Secrets) -> Option<String> {
    let presented = header?.strip_prefix("Bearer ")?.trim();
    if presented.is_empty() {
        return None;
    }
    let mut found = None;
    for (agent, token) in &secrets.tokens {
        if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
            found = Some(agent.clone());
        }
    }
    found
}

/// Applies the rules from the spec. `now` is the server clock in Unix
/// milliseconds and `retention_ms` the raw retention window.
pub fn validate(
    body: IngestBody,
    token_host: &str,
    now: u64,
    retention_ms: u64,
) -> Result<Checked, Reject> {
    if body.samples.len() > MAX_BATCH {
        return Err(Reject::TooLarge(format!(
            "at most {MAX_BATCH} samples per request, got {}",
            body.samples.len()
        )));
    }
    if body.events.len() > MAX_BATCH {
        return Err(Reject::TooLarge(format!(
            "at most {MAX_BATCH} events per request, got {}",
            body.events.len()
        )));
    }
    if body.host != token_host {
        return Err(Reject::Bad(format!(
            "host `{}` does not match the host this token belongs to",
            body.host
        )));
    }
    if body.samples.is_empty() && body.events.is_empty() {
        return Err(Reject::Bad("request has no samples and no events".into()));
    }
    if let Some(agent) = &body.agent
        && (agent.is_empty() || agent.len() > MAX_TEXT_LEN)
    {
        return Err(Reject::Bad("`agent` must be 1 to 256 bytes".into()));
    }

    for pair in body.samples.windows(2) {
        if pair[1].timestamp <= pair[0].timestamp {
            return Err(Reject::Bad(
                "samples must be in strictly increasing timestamp order".into(),
            ));
        }
    }
    for sample in &body.samples {
        check_sample_names(sample)?;
    }
    for event in &body.events {
        for (field, text) in [
            ("rule", &event.rule),
            ("scope", &event.scope),
            ("metric", &event.metric),
        ] {
            if text.is_empty() || text.len() > MAX_TEXT_LEN {
                return Err(Reject::Bad(format!(
                    "event `{field}` must be 1 to {MAX_TEXT_LEN} bytes"
                )));
            }
        }
    }

    let oldest = now.saturating_sub(retention_ms);
    let newest = now.saturating_add(MAX_FUTURE_MS);
    let total = body.samples.len();
    let samples: Vec<Sample> = body
        .samples
        .into_iter()
        .filter(|s| (oldest..=newest).contains(&s.timestamp))
        .collect();
    let rejected = total - samples.len();
    if total > 0 && samples.is_empty() {
        return Err(Reject::Bad(
            "every sample was rejected: timestamps must be within the retention window and at most 5 minutes in the future".into(),
        ));
    }

    Ok(Checked {
        agent: body.agent,
        samples,
        events: body.events,
        rejected,
    })
}

fn check_sample_names(sample: &Sample) -> Result<(), Reject> {
    let bad = |what: &str, name: &str, reason: &str| {
        Reject::Bad(format!("{what} name `{name}` {reason}"))
    };
    if let Err(reason) = check_name(&sample.host.name) {
        return Err(bad("host", &sample.host.name, reason));
    }
    for disk in &sample.host.disks {
        if let Err(reason) = check_name(&disk.mount) {
            return Err(bad("mount", &disk.mount, reason));
        }
    }
    for service in &sample.services {
        if let Err(reason) = check_name(&service.name) {
            return Err(bad("service", &service.name, reason));
        }
        if service.unit.len() > MAX_TEXT_LEN {
            return Err(Reject::Bad(format!(
                "service `{}`: unit name is longer than {MAX_TEXT_LEN} bytes",
                service.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninja_monitor::alerts::EventKind;
    use ninja_monitor::sample::{HostSample, ServiceSample, ServiceState};

    const NOW: u64 = 1_760_000_000_000;
    const HOUR: u64 = 3_600_000;

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
            services: vec![ServiceSample {
                name: "svc".into(),
                unit: "svc.service".into(),
                state: ServiceState::Down,
                cpu_percent: None,
                mem_current_bytes: None,
                mem_anon_bytes: None,
                mem_file_bytes: None,
                mem_peak_bytes: None,
                oom_kills: None,
                pids: None,
                restarts_seen: 0,
                disk_bytes: None,
                disk_measured_at: None,
            }],
        }
    }

    fn event() -> AlertEvent {
        AlertEvent {
            timestamp: NOW,
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

    fn body(samples: Vec<Sample>, events: Vec<AlertEvent>) -> IngestBody {
        IngestBody {
            agent: Some("ninja-monitor/0.1.0".into()),
            host: "vps".into(),
            samples,
            events,
        }
    }

    fn bad(result: Result<Checked, Reject>) -> String {
        match result {
            Err(Reject::Bad(msg)) => msg,
            other => panic!("expected Bad, got {other:?}"),
        }
    }

    #[test]
    fn authenticates_by_token() {
        let secrets = Secrets {
            tokens: vec![("vps".into(), "abc".into()), ("home".into(), "xyz".into())],
            ..Secrets::default()
        };
        assert_eq!(
            authenticate(Some("Bearer abc"), &secrets).as_deref(),
            Some("vps")
        );
        assert_eq!(
            authenticate(Some("Bearer xyz "), &secrets).as_deref(),
            Some("home")
        );
        assert_eq!(authenticate(Some("Bearer ab"), &secrets), None);
        assert_eq!(authenticate(Some("Bearer "), &secrets), None);
        assert_eq!(authenticate(Some("Basic abc"), &secrets), None);
        assert_eq!(authenticate(Some("abc"), &secrets), None);
        assert_eq!(authenticate(None, &secrets), None);
        assert_eq!(authenticate(Some("Bearer abc"), &Secrets::default()), None);
    }

    #[test]
    fn accepts_a_good_batch() {
        let checked = validate(
            body(
                vec![sample(NOW - 30_000), sample(NOW - 15_000)],
                vec![event()],
            ),
            "vps",
            NOW,
            HOUR,
        )
        .unwrap();
        assert_eq!(checked.samples.len(), 2);
        assert_eq!(checked.events.len(), 1);
        assert_eq!(checked.rejected, 0);
        assert_eq!(checked.agent.as_deref(), Some("ninja-monitor/0.1.0"));
        // Events alone are fine.
        assert!(validate(body(vec![], vec![event()]), "vps", NOW, HOUR).is_ok());
    }

    #[test]
    fn size_limits() {
        let many: Vec<_> = (0..=MAX_BATCH as u64)
            .map(|i| sample(NOW - i))
            .rev()
            .collect();
        assert!(matches!(
            validate(body(many, vec![]), "vps", NOW, HOUR),
            Err(Reject::TooLarge(_))
        ));
        let events = vec![event(); MAX_BATCH + 1];
        assert!(matches!(
            validate(body(vec![], events), "vps", NOW, HOUR),
            Err(Reject::TooLarge(_))
        ));
        let exactly: Vec<_> = (0..MAX_BATCH as u64)
            .map(|i| sample(NOW - i))
            .rev()
            .collect();
        assert!(validate(body(exactly, vec![]), "vps", NOW, HOUR).is_ok());
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(
            bad(validate(body(vec![sample(NOW)], vec![]), "home", NOW, HOUR))
                .contains("does not match")
        );
        assert!(bad(validate(body(vec![], vec![]), "vps", NOW, HOUR)).contains("no samples"));
        assert!(
            bad(validate(
                body(vec![sample(NOW), sample(NOW)], vec![]),
                "vps",
                NOW,
                HOUR
            ))
            .contains("increasing")
        );
        assert!(
            bad(validate(
                body(vec![sample(NOW), sample(NOW - 1)], vec![]),
                "vps",
                NOW,
                HOUR
            ))
            .contains("increasing")
        );
        let mut b = body(vec![sample(NOW)], vec![]);
        b.agent = Some(String::new());
        assert!(bad(validate(b, "vps", NOW, HOUR)).contains("agent"));
    }

    #[test]
    fn rejects_bad_names() {
        let mut s = sample(NOW);
        s.host.name = "x".repeat(65);
        assert!(bad(validate(body(vec![s], vec![]), "vps", NOW, HOUR)).contains("host name"));
        let mut s = sample(NOW);
        s.services[0].name = "bad\nname".into();
        assert!(bad(validate(body(vec![s], vec![]), "vps", NOW, HOUR)).contains("service name"));
        let mut s = sample(NOW);
        s.host.disks.push(ninja_monitor::sample::DiskSample {
            mount: String::new(),
            total_bytes: None,
            used_bytes: None,
            avail_bytes: None,
            used_percent: None,
        });
        assert!(bad(validate(body(vec![s], vec![]), "vps", NOW, HOUR)).contains("mount name"));
        let mut e = event();
        e.rule = String::new();
        assert!(bad(validate(body(vec![], vec![e]), "vps", NOW, HOUR)).contains("`rule`"));
    }

    #[test]
    fn time_window() {
        // One too old, one too far ahead, one fine.
        let checked = validate(
            body(
                vec![
                    sample(NOW - HOUR - 1),
                    sample(NOW),
                    sample(NOW + MAX_FUTURE_MS + 1),
                ],
                vec![],
            ),
            "vps",
            NOW,
            HOUR,
        )
        .unwrap();
        assert_eq!(checked.samples.len(), 1);
        assert_eq!(checked.samples[0].timestamp, NOW);
        assert_eq!(checked.rejected, 2);
        // Boundaries are inclusive.
        let edges = validate(
            body(
                vec![sample(NOW - HOUR), sample(NOW + MAX_FUTURE_MS)],
                vec![],
            ),
            "vps",
            NOW,
            HOUR,
        )
        .unwrap();
        assert_eq!(edges.rejected, 0);
        // All rejected is an error, even with events attached.
        assert!(
            bad(validate(
                body(vec![sample(1)], vec![event()]),
                "vps",
                NOW,
                HOUR
            ))
            .contains("every sample was rejected")
        );
    }
}
