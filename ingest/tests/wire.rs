//! The wire format is the agent's own JSON. These tests pin down that it
//! round-trips and that a newer agent's extra fields are ignored.

use ninja_monitor::alerts::{AlertEvent, EventKind};
use ninja_monitor::sample::{DiskSample, HostSample, Sample, ServiceSample, ServiceState};
use serde_json::{Value, json};

fn sample() -> Sample {
    Sample {
        timestamp: 1_760_003_585_000,
        host: HostSample {
            name: "vps".into(),
            cpu_percent: Some(12.5),
            cores: Some(2),
            load1: Some(0.5),
            load5: None,
            load15: Some(0.1),
            mem_total_bytes: Some(4_000_000_000),
            mem_available_bytes: Some(3_000_000_000),
            mem_used_bytes: Some(1_000_000_000),
            mem_cache_bytes: None,
            swap_total_bytes: Some(0),
            swap_used_bytes: Some(0),
            uptime_secs: Some(86_400),
            disks: vec![DiskSample {
                mount: "/".into(),
                total_bytes: Some(40_000_000_000),
                used_bytes: Some(10_000_000_000),
                avail_bytes: Some(30_000_000_000),
                used_percent: Some(25.0),
            }],
        },
        services: vec![ServiceSample {
            name: "riot-proxy".into(),
            unit: "riot-proxy.service".into(),
            state: ServiceState::Running,
            cpu_percent: None,
            mem_current_bytes: Some(618_397_696),
            mem_anon_bytes: Some(208_506_880),
            mem_file_bytes: Some(403_083_264),
            mem_peak_bytes: None,
            oom_kills: Some(0),
            pids: Some(14),
            restarts_seen: 1,
            disk_bytes: Some(73_728),
            disk_measured_at: Some(1_760_003_000_000),
        }],
    }
}

#[test]
fn sample_round_trips() {
    let original = serde_json::to_value(sample()).unwrap();
    let parsed: Sample = serde_json::from_value(original.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
    assert_eq!(original["services"][0]["state"], "running");
    assert_eq!(original["host"]["load5"], Value::Null);
}

#[test]
fn unknown_fields_are_ignored() {
    let mut value = serde_json::to_value(sample()).unwrap();
    value["future_field"] = json!(1);
    value["host"]["future_host_field"] = json!("x");
    value["services"][0]["future_service_field"] = json!(true);
    let parsed: Sample = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.services[0].name, "riot-proxy");
}

#[test]
fn alert_event_round_trips() {
    let event = AlertEvent {
        timestamp: 1_760_003_585_000,
        rule: "riot-proxy down".into(),
        scope: "service:riot-proxy".into(),
        metric: "down".into(),
        kind: EventKind::Resolved,
        value: Some(0.0),
        op: None,
        threshold: None,
        held_secs: 95,
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["kind"], "resolved");
    let parsed: AlertEvent = serde_json::from_value(value).unwrap();
    assert_eq!(parsed, event);
}
