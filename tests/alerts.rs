//! Alerts end to end, with a local stub standing in for Discord.

mod common;

use std::fs;
use std::time::Duration;

use serde_json::Value;

use common::{App, Stub, wait_until, write_atomic};
use ninja_monitor::alerts::notify::{Discord, http_agent};
use ninja_monitor::config::Secrets;

const RULES: &str = r#"
    [discord]
    enabled = true

    [[alert]]
    name = "riot-proxy memory high"
    scope = "service:riot-proxy"
    metric = "mem_anon_bytes"
    op = ">"
    threshold = 300000000
    for = "300ms"

    [[alert]]
    name = "cloudflared OOM kill"
    scope = "service:cloudflared"
    metric = "oom_kill"

    [[alert]]
    name = "site down"
    scope = "service:site"
    metric = "down"
"#;

fn discord_secrets(stub: &Stub) -> Secrets {
    Secrets {
        discord_webhook: Some(format!("{}/api/webhooks/1/token", stub.url)),
        ..Secrets::default()
    }
}

fn rule<'a>(alerts: &'a Value, name: &str) -> &'a Value {
    alerts["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap()
}

#[test]
fn discord_gets_startup_firing_resolved_and_oom() {
    let stub = Stub::start();
    let app = App::start_full("100ms", "1h", RULES, discord_secrets(&stub));

    let started = stub.wait_for_content("STARTED");
    assert!(started.contains("[test-host]"), "{started}");
    // `site` has no cgroup, so its down rule fires straight away.
    stub.wait_for_content("site down: service is down");

    let request = &stub.requests()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/webhooks/1/token");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));

    let (_, alerts) = app.get("/alerts");
    assert_eq!(rule(&alerts, "riot-proxy memory high")["state"], "ok");
    assert_eq!(
        rule(&alerts, "riot-proxy memory high")["value"],
        208506880.0
    );
    assert_eq!(rule(&alerts, "site down")["state"], "firing");

    // Push riot-proxy's anonymous memory over the threshold.
    let stat_path = app.path("cgroup/system.slice/riot-proxy.service/memory.stat");
    let original = fs::read_to_string(&stat_path).unwrap();
    write_atomic(
        &stat_path,
        &original.replace("anon 208506880", "anon 400000000"),
    );
    wait_until("rule to go pending or firing", || {
        let (_, alerts) = app.get("/alerts");
        (rule(&alerts, "riot-proxy memory high")["state"] != "ok").then_some(())
    });
    let firing = stub.wait_for_content("FIRING** [test-host] riot-proxy memory high");
    assert!(
        firing.contains("mem_anon_bytes = 381.5 MiB (threshold > 286.1 MiB)"),
        "{firing}"
    );
    let (_, alerts) = app.get("/alerts");
    let state = rule(&alerts, "riot-proxy memory high");
    assert_eq!(state["state"], "firing");
    assert!(state["since"].as_u64().is_some());
    let newest = &alerts["events"][0];
    assert_eq!(newest["rule"], "riot-proxy memory high");
    assert_eq!(newest["kind"], "firing");

    // Back under the threshold.
    write_atomic(&stat_path, &original);
    let resolved = stub.wait_for_content("RESOLVED** [test-host] riot-proxy memory high");
    assert!(
        resolved.contains("mem_anon_bytes = 198.8 MiB"),
        "{resolved}"
    );
    let (_, alerts) = app.get("/alerts");
    assert_eq!(rule(&alerts, "riot-proxy memory high")["state"], "ok");
    assert_eq!(alerts["events"][0]["kind"], "resolved");

    // An OOM kill is a one-off event.
    let events_path = app.path("cgroup/system.slice/cloudflared.service/memory.events");
    let events = fs::read_to_string(&events_path).unwrap();
    write_atomic(&events_path, &events.replace("oom_kill 0", "oom_kill 1"));
    let oom = stub.wait_for_content("cloudflared OOM kill");
    assert!(oom.contains("count now 1"), "{oom}");

    // Exactly one of each, nothing repeated.
    let contents = stub.contents();
    let count = |needle: &str| contents.iter().filter(|c| c.contains(needle)).count();
    assert_eq!(count("STARTED"), 1);
    assert_eq!(count("FIRING** [test-host] riot-proxy memory high"), 1);
    assert_eq!(count("RESOLVED** [test-host] riot-proxy memory high"), 1);
    assert_eq!(count("site down"), 1);
}

#[test]
fn startup_message_can_be_switched_off() {
    let stub = Stub::start();
    let rules = RULES.replace("enabled = true", "enabled = true\nstartup_message = false");
    let _app = App::start_full("100ms", "1h", &rules, discord_secrets(&stub));
    stub.wait_for_content("site down");
    assert!(!stub.contents().iter().any(|c| c.contains("STARTED")));
}

#[test]
fn runs_without_webhook_even_when_enabled() {
    let app = App::start_full("100ms", "1h", RULES, Secrets::default());
    let alerts = wait_until("site down to fire", || {
        let (_, alerts) = app.get("/alerts");
        (rule(&alerts, "site down")["state"] == "firing").then_some(alerts)
    });
    assert_eq!(alerts["events"][0]["rule"], "site down");
}

#[test]
fn alerts_endpoint_without_rules() {
    let app = App::start("1h");
    let (status, body) = app.get("/alerts");
    assert_eq!(status, 200);
    assert_eq!(body["rules"], serde_json::json!([]));
    assert_eq!(body["events"], serde_json::json!([]));
}

#[test]
fn heartbeat_pings_on_interval() {
    let stub = Stub::start();
    let secrets = Secrets {
        heartbeat_url: Some(format!("{}/ping/abc", stub.url)),
        ..Secrets::default()
    };
    let extra = "[heartbeat]\nenabled = true\ninterval = \"100ms\"";
    let _app = App::start_full("1h", "1h", extra, secrets);
    wait_until("three heartbeats", || {
        (stub.requests().len() >= 3).then_some(())
    });
    for request in stub.requests() {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/ping/abc");
    }
}

#[test]
fn heartbeat_survives_failures() {
    let stub = Stub::start();
    stub.reply_next(500, "");
    stub.reply_next(503, "");
    let secrets = Secrets {
        heartbeat_url: Some(format!("{}/ping", stub.url)),
        ..Secrets::default()
    };
    let extra = "[heartbeat]\nenabled = true\ninterval = \"100ms\"";
    let _app = App::start_full("1h", "1h", extra, secrets);
    wait_until("pings after failures", || {
        (stub.requests().len() >= 4).then_some(())
    });
}

fn discord(stub: &Stub) -> Discord {
    Discord::with_backoff(
        http_agent(),
        format!("{}/hook", stub.url),
        Duration::from_millis(10),
    )
}

#[tokio::test]
async fn delivery_waits_out_rate_limits() {
    let stub = Stub::start();
    stub.reply_next(
        429,
        r#"{"message":"slow down","retry_after":0.2,"global":false}"#,
    );
    let started = std::time::Instant::now();
    assert!(discord(&stub).deliver("hello").await);
    assert!(started.elapsed() >= Duration::from_millis(200));
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn delivery_retries_server_errors_three_times() {
    let stub = Stub::start();
    for _ in 0..4 {
        stub.reply_next(502, "");
    }
    assert!(!discord(&stub).deliver("hello").await);
    assert_eq!(stub.requests().len(), 4);

    let stub = Stub::start();
    stub.reply_next(500, "");
    stub.reply_next(500, "");
    assert!(discord(&stub).deliver("hello").await);
    assert_eq!(stub.requests().len(), 3);
}

#[tokio::test]
async fn delivery_gives_up_on_client_errors() {
    let stub = Stub::start();
    stub.reply_next(404, r#"{"message":"Unknown Webhook"}"#);
    assert!(!discord(&stub).deliver("hello").await);
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test]
async fn delivery_retries_when_nothing_listens() {
    // Bind then drop to get a port with nothing behind it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let discord = Discord::with_backoff(
        http_agent(),
        format!("http://127.0.0.1:{port}/hook"),
        Duration::from_millis(5),
    );
    assert!(!discord.deliver("hello").await);
}
