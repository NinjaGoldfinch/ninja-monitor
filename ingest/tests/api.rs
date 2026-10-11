//! End-to-end tests for the ingest route and the read API, including the
//! conformance check against the agent's own history output.

mod common;

use common::{Server, TOKEN_HOME, TOKEN_VPS, event, now_ms, sample};
use serde_json::{Value, json};

use ninja_monitor::alerts::EventKind;
use ninja_monitor::config::{Config as AgentConfig, Scope};
use ninja_monitor::history;
use ninja_monitor::sample::Sample;
use ninja_monitor::store::{Range, Store};

/// `count` samples 15 seconds apart, the newest `age_ms` ago.
fn series(host: &str, count: u64, age_ms: u64) -> Vec<Sample> {
    let now = now_ms();
    let newest = now - age_ms;
    (0..count)
        .map(|i| sample(host, newest - (count - 1 - i) * 15_000, i))
        .collect()
}

#[test]
fn healthz_and_unknown_routes() {
    let server = Server::start("");
    let (status, body) = server.get("/healthz");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["hosts"], 0);
    assert!(body["db_bytes"].as_u64().unwrap() > 0);
    let (status, body) = server.get("/nope");
    assert_eq!(status, 404);
    assert_eq!(body["error"], "not found");
}

#[test]
fn ingest_requires_a_valid_token_for_the_named_host() {
    let server = Server::start("");
    let body = json!({ "host": "vps", "samples": series("vps", 1, 0) });
    let (status, _) = server.post(None, &body);
    assert_eq!(status, 401);
    let (status, _) = server.post(Some("wrong"), &body);
    assert_eq!(status, 401);
    // The right token for the wrong host.
    let (status, err) = server.post(Some(TOKEN_HOME), &body);
    assert_eq!(status, 400, "{err}");
    assert!(err["error"].as_str().unwrap().contains("does not match"));
    let (status, ok) = server.post(Some(TOKEN_VPS), &body);
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["accepted"], 1);
    assert_eq!(ok["duplicates"], 0);
    assert_eq!(ok["rejected"], 0);
    assert_eq!(ok["events_accepted"], 0);
}

#[test]
fn ingest_rejects_malformed_and_oversized_requests() {
    let server = Server::start("");
    let (status, err) = server.post(Some(TOKEN_VPS), &json!({ "host": "vps" }));
    assert_eq!(status, 400, "{err}");
    let (status, _) = server.post(Some(TOKEN_VPS), &json!({ "samples": [] }));
    assert_eq!(status, 400);
    let (status, _) = server.post(Some(TOKEN_VPS), &json!("not an object"));
    assert_eq!(status, 400);

    let too_many = series("vps", 201, 0);
    let (status, err) = server.post(
        Some(TOKEN_VPS),
        &json!({ "host": "vps", "samples": too_many }),
    );
    assert_eq!(status, 413, "{err}");

    // Just over 2 MiB of body is refused by the body limit, before parsing.
    let padding = "x".repeat(2 * 1024 * 1024 + 1024);
    let (status, _) = server.post(Some(TOKEN_VPS), &json!({ "host": "vps", "pad": padding }));
    assert_eq!(status, 413);

    let mut backwards = series("vps", 2, 0);
    backwards.reverse();
    let (status, err) = server.post(
        Some(TOKEN_VPS),
        &json!({ "host": "vps", "samples": backwards }),
    );
    assert_eq!(status, 400);
    assert!(err["error"].as_str().unwrap().contains("increasing"));

    // Far in the past: rejected outright.
    let ancient = vec![sample("vps", 1_000_000, 0)];
    let (status, err) = server.post(
        Some(TOKEN_VPS),
        &json!({ "host": "vps", "samples": ancient }),
    );
    assert_eq!(status, 400);
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("every sample was rejected")
    );

    // Mixed: the good one is stored and the bad one counted.
    let mixed = vec![sample("vps", 1_000_000, 0), sample("vps", now_ms(), 1)];
    let (status, ok) = server.post(Some(TOKEN_VPS), &json!({ "host": "vps", "samples": mixed }));
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["accepted"], 1);
    assert_eq!(ok["rejected"], 1);
}

#[test]
fn duplicates_are_ignored_and_unknown_fields_tolerated() {
    let server = Server::start("");
    let samples = series("vps", 3, 0);
    let first = server.push(TOKEN_VPS, "vps", &samples, &[]);
    assert_eq!(first["accepted"], 3);
    let again = server.push(TOKEN_VPS, "vps", &samples[1..], &[]);
    assert_eq!(again["accepted"], 0);
    assert_eq!(again["duplicates"], 2);

    let mut with_extra = serde_json::to_value(&samples[0]).unwrap();
    with_extra["timestamp"] = json!(samples[2].timestamp + 15_000);
    with_extra["brand_new_field"] = json!(true);
    with_extra["host"]["other_new_field"] = json!(1);
    let (status, ok) = server.post(
        Some(TOKEN_VPS),
        &json!({ "host": "vps", "samples": [with_extra], "future": "ignored" }),
    );
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["accepted"], 1);

    let (_, health) = server.get("/healthz");
    assert_eq!(health["hosts"], 1);
}

#[test]
fn hosts_and_stats_views() {
    let server = Server::start("");
    let (status, body) = server.get("/hosts");
    assert_eq!(status, 200);
    let hosts = body["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2, "{body}");
    assert!(
        hosts
            .iter()
            .all(|h| h["status"] == "unknown" && h["last_seen"].is_null())
    );

    let (status, body) = server.get("/hosts/vps/stats");
    assert_eq!(status, 503, "{body}");
    let (status, body) = server.get("/hosts/nope/stats");
    assert_eq!(status, 404, "{body}");

    let samples = series("vps", 4, 0);
    server.push(TOKEN_VPS, "vps", &samples, &[]);

    let (status, body) = server.get("/hosts");
    assert_eq!(status, 200);
    let vps = body["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(vps["status"], "reporting");
    assert!(vps["last_seen"].as_u64().unwrap() >= samples[3].timestamp);
    assert_eq!(vps["agent"], "ninja-monitor/0.1.0-test");
    assert!(vps["silent_since"].is_null());

    let (status, body) = server.get("/hosts/vps/stats");
    assert_eq!(status, 200, "{body}");
    let received_at = body["received_at"].as_u64().unwrap();
    assert!(received_at >= samples[3].timestamp);
    let mut body = body;
    body.as_object_mut().unwrap().remove("received_at");
    assert_eq!(body, serde_json::to_value(&samples[3]).unwrap());

    let (status, body) = server.get("/stats");
    assert_eq!(status, 200);
    let all = body["hosts"].as_array().unwrap();
    assert_eq!(all.len(), 2);
    let home = all.iter().find(|h| h["name"] == "home").unwrap();
    assert_eq!(home["status"], "unknown");
    assert!(home["sample"].is_null());
    let vps = all.iter().find(|h| h["name"] == "vps").unwrap();
    assert_eq!(vps["sample"]["timestamp"], samples[3].timestamp);
    assert_eq!(vps["sample"]["services"].as_array().unwrap().len(), 2);
}

#[test]
fn silence_is_derived_from_last_seen() {
    let server = Server::start("silence.after = \"1ms\"");
    server.push(TOKEN_VPS, "vps", &series("vps", 1, 0), &[]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (_, body) = server.get("/hosts");
    let vps = body["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(vps["status"], "silent", "{vps}");
    assert!(vps["silent_since"].as_u64().unwrap() > vps["last_seen"].as_u64().unwrap());
}

#[test]
fn history_matches_the_agent_exactly() {
    let server = Server::start("");
    // 50 samples, old enough that the newest is well inside the window.
    let samples = series("vps", 50, 60_000);
    // Two batches, with an overlap, as a catch-up would send.
    server.push(TOKEN_VPS, "vps", &samples[..30], &[]);
    server.push(TOKEN_VPS, "vps", &samples[25..], &[]);

    // The agent's view of the same samples.
    let agent_config = AgentConfig::parse(
        r#"
        [sampling]
        interval = "15s"
        history = "24h"
        [host]
        name = "vps"
        mounts = ["/"]
        [[service]]
        name = "riot-proxy"
        unit = "riot-proxy.service"
        [[service]]
        name = "cloudflared"
        unit = "cloudflared.service"
        "#,
    )
    .unwrap();
    let agent = Store::new(&agent_config);
    for s in &samples {
        agent.push(std::sync::Arc::new(s.clone()));
    }

    for (scope, max_points) in [
        ("host", 720),
        ("host", 7),
        ("service:riot-proxy", 720),
        ("service:riot-proxy", 12),
        ("service:cloudflared", 3),
    ] {
        let (status, body) = server.get(&format!(
            "/hosts/vps/history?scope={scope}&minutes=60&max_points={max_points}"
        ));
        assert_eq!(status, 200, "{body}");
        let (from, to) = (body["from"].as_u64().unwrap(), body["to"].as_u64().unwrap());
        assert_eq!(body["scope"], scope);
        assert_eq!(body["minutes"], 60);

        let expected = match agent.range(&Scope::try_from(scope.to_string()).unwrap(), from, to) {
            Some(Range::Host { points, disks }) => {
                serde_json::to_value(history::host(&points, &disks, max_points)).unwrap()
            }
            Some(Range::Service(points)) => {
                serde_json::to_value(history::service(&points, max_points)).unwrap()
            }
            None => panic!("agent has no scope {scope}"),
        };
        assert!(
            expected["t"].as_array().unwrap().len() >= 3,
            "window too small to be meaningful"
        );
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(
                &body[key], value,
                "{scope} max_points={max_points} field {key}"
            );
        }
        // And nothing extra beyond the four envelope fields.
        assert_eq!(
            body.as_object().unwrap().len(),
            expected.as_object().unwrap().len() + 4,
            "{scope}: {body}"
        );
    }
}

#[test]
fn history_windows_and_errors() {
    let server = Server::start("storage.retention = \"2h\"");
    let samples = series("vps", 10, 0);
    server.push(TOKEN_VPS, "vps", &samples, &[]);

    // Default window is 60 minutes; minutes beyond retention are capped.
    let (status, body) = server.get("/hosts/vps/history?scope=host");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["minutes"], 60);
    assert_eq!(body["t"].as_array().unwrap().len(), 10);
    let (_, body) = server.get("/hosts/vps/history?scope=host&minutes=100000");
    assert_eq!(body["minutes"], 120);

    // Explicit from and to.
    let (from, to) = (samples[2].timestamp, samples[5].timestamp);
    let (status, body) = server.get(&format!(
        "/hosts/vps/history?scope=service:riot-proxy&from={from}&to={to}"
    ));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["from"], from);
    assert_eq!(body["to"], to);
    assert_eq!(body["t"].as_array().unwrap().len(), 4);
    assert_eq!(body["minutes"], 1);

    // max_points is capped, not refused.
    let (status, body) = server.get("/hosts/vps/history?scope=host&max_points=999999");
    assert_eq!(status, 200, "{body}");

    // A configured host that has never reported has empty history.
    let (status, body) = server.get("/hosts/home/history?scope=host");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["t"], json!([]));
    assert_eq!(body["disks"], json!([]));

    let hour = 3_600_000;
    for (path, expected) in [
        ("/hosts/vps/history", 400),
        ("/hosts/vps/history?scope=host&minutes=0", 400),
        ("/hosts/vps/history?scope=host&minutes=abc", 400),
        ("/hosts/vps/history?scope=host&max_points=0", 400),
        ("/hosts/vps/history?scope=host&from=5", 400),
        ("/hosts/vps/history?scope=host&from=10&to=5", 400),
        ("/hosts/vps/history?scope=host&from=1&to=2&minutes=5", 400),
        ("/hosts/vps/history?scope=host&from=a&to=b", 400),
        ("/hosts/vps/history?scope=service:nope", 404),
        ("/hosts/vps/history?scope=galaxy", 404),
        ("/hosts/home/history?scope=service:riot-proxy", 404),
        ("/hosts/nope/history?scope=host", 404),
    ] {
        let (status, body) = server.get(path);
        assert_eq!(status, expected, "{path}: {body}");
        assert!(body["error"].is_string(), "{path}: {body}");
    }
    let too_long = format!("/hosts/vps/history?scope=host&from=0&to={}", 3 * hour);
    let (status, body) = server.get(&too_long);
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("retention"));
}

#[test]
fn alert_events_are_stored_and_listed_newest_first() {
    let server = Server::start("");
    let now = now_ms();
    let events = vec![
        event(now - 30_000, "riot-proxy memory high", EventKind::Firing),
        event(now - 10_000, "riot-proxy memory high", EventKind::Resolved),
    ];
    let ok = server.push(TOKEN_VPS, "vps", &[], &events);
    assert_eq!(ok["accepted"], 0);
    assert_eq!(ok["events_accepted"], 2);
    // Re-sending is harmless.
    let ok = server.push(TOKEN_VPS, "vps", &[], &events);
    assert_eq!(ok["events_accepted"], 0);

    let (status, body) = server.get("/hosts/vps/alerts");
    assert_eq!(status, 200, "{body}");
    let listed = body["events"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], serde_json::to_value(&events[1]).unwrap());
    assert_eq!(listed[1], serde_json::to_value(&events[0]).unwrap());
    assert!(body.get("rules").is_none());

    let (_, body) = server.get("/hosts/vps/alerts?limit=1");
    assert_eq!(body["events"].as_array().unwrap().len(), 1);
    let (status, _) = server.get("/hosts/vps/alerts?limit=0");
    assert_eq!(status, 400);
    let (status, body) = server.get("/hosts/home/alerts");
    assert_eq!(status, 200);
    assert_eq!(body["events"], json!([]));
    let (status, _) = server.get("/hosts/nope/alerts");
    assert_eq!(status, 404);
}

#[test]
fn restart_loses_nothing() {
    let server = Server::start("");
    let samples = series("vps", 5, 0);
    server.push(
        TOKEN_VPS,
        "vps",
        &samples,
        &[event(now_ms(), "r", EventKind::Firing)],
    );
    server.push(TOKEN_HOME, "home", &series("home", 2, 0), &[]);

    let server = server.restart("");
    let (_, health) = server.get("/healthz");
    assert_eq!(health["hosts"], 2);
    let (status, body) = server.get("/hosts/vps/stats");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["timestamp"], samples[4].timestamp);
    let (_, body) = server.get("/hosts/vps/history?scope=host");
    assert_eq!(body["t"].as_array().unwrap().len(), 5);
    let (_, body) = server.get("/hosts/vps/alerts");
    assert_eq!(body["events"].as_array().unwrap().len(), 1);
    let (_, body) = server.get("/hosts/home/history?scope=service:riot-proxy");
    assert_eq!(body["t"].as_array().unwrap().len(), 2);
}

#[test]
fn cors_only_for_listed_origins_on_read_routes() {
    let server = Server::start("server.cors_origins = [\"https://ninjagoldfinch.com\"]");
    let allowed = [("Origin", "https://ninjagoldfinch.com")];
    let r = server.request("GET", "/hosts", &allowed, None);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://ninjagoldfinch.com")
    );
    assert_eq!(r.header("vary"), Some("Origin"));

    let r = server.request("OPTIONS", "/hosts/vps/history", &allowed, None);
    assert_eq!(r.status, 204);
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://ninjagoldfinch.com")
    );
    assert_eq!(
        r.header("access-control-allow-methods"),
        Some("GET, OPTIONS")
    );

    let r = server.request("GET", "/hosts", &[("Origin", "https://evil.example")], None);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), None);

    let r = server.request("GET", "/hosts", &[], None);
    assert_eq!(r.header("access-control-allow-origin"), None);

    let auth = format!("Bearer {TOKEN_VPS}");
    let body: Value = json!({ "host": "vps", "samples": series("vps", 1, 0) });
    let r = server.request(
        "POST",
        "/v1/ingest",
        &[
            ("Origin", "https://ninjagoldfinch.com"),
            ("Authorization", &auth),
        ],
        Some(&body),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("access-control-allow-origin"), None);

    let plain = Server::start("");
    let r = plain.request("GET", "/hosts", &allowed, None);
    assert_eq!(r.header("access-control-allow-origin"), None);
}

/// Fills 30 days of 15 second samples for one host and times the widest
/// query. Run by hand: `cargo test -p ninja-ingest --test api -- --ignored --nocapture`.
#[test]
#[ignore]
fn full_retention_query_timing() {
    use ninja_ingest::db::Db;
    use ninja_ingest::store;
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ingest.db");
    let db = Db::open(&path).unwrap();
    let now = now_ms();
    let count: u64 = 30 * 24 * 60 * 4;
    let start = now - count * 15_000;
    let fill = Instant::now();
    for batch in 0..(count / 200) {
        let samples: Vec<Sample> = (0..200)
            .map(|j| {
                let i = batch * 200 + j;
                sample("vps", start + i * 15_000, i)
            })
            .collect();
        db.call_sync(|c| store::insert_batch(c, "vps", Some("bench"), now, &samples, &[]))
            .unwrap();
    }
    let fill = fill.elapsed();
    let size = std::fs::metadata(&path).unwrap().len();
    drop(db);

    let server = Server::start_on(dir, path, "");
    for scope in ["host", "service:riot-proxy"] {
        let t = Instant::now();
        let (status, body) = server.get(&format!(
            "/hosts/vps/history?scope={scope}&minutes=43200&max_points=2000"
        ));
        let took = t.elapsed();
        assert_eq!(status, 200, "{body}");
        println!(
            "{scope}: {} points from {count} samples in {took:?}",
            body["t"].as_array().unwrap().len()
        );
    }
    println!(
        "fill {count} samples: {fill:?}; database {} MiB",
        size / 1024 / 1024
    );
}
