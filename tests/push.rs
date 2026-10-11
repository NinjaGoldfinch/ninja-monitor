//! The pusher end to end against a stub standing in for ninja-ingest, with
//! a second stub as Discord for the unreachable alert.

mod common;

use std::net::TcpListener;
use std::time::Duration;

use serde_json::Value;

use common::{App, Stub, wait_until};
use ninja_monitor::config::Secrets;

const TOKEN: &str = "test-push-token-0123456789abcdef0123456789abcdef";

fn push_config(url: &str, extra: &str) -> String {
    format!(
        "[push]\nenabled = true\nurl = \"{url}/v1/ingest\"\nflush_interval = \"100ms\"\nbuffer = \"10s\"\n{extra}"
    )
}

fn secrets(discord: Option<&Stub>) -> Secrets {
    Secrets {
        push_token: Some(TOKEN.into()),
        discord_webhook: discord.map(|s| format!("{}/api/webhooks/1/token", s.url)),
        ..Secrets::default()
    }
}

/// A free loopback port nothing listens on, until the test binds it.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn pushes_each_sample_with_the_token_and_host() {
    let ingest = Stub::start();
    let _app = App::start_full("100ms", "1h", &push_config(&ingest.url, ""), secrets(None));

    let requests = wait_until("three pushes", || {
        let r = ingest.requests();
        (r.len() >= 3).then_some(r)
    });
    let mut last_ts = 0;
    let mut total = 0;
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/ingest");
        assert_eq!(
            request.header("authorization"),
            Some(&*format!("Bearer {TOKEN}"))
        );
        assert_eq!(request.header("content-type"), Some("application/json"));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["host"], "test-host");
        assert!(
            body["agent"]
                .as_str()
                .unwrap()
                .starts_with("ninja-monitor/")
        );
        assert_eq!(body["events"], serde_json::json!([]));
        let samples = body["samples"].as_array().unwrap();
        assert!(!samples.is_empty());
        for s in samples {
            let ts = s["timestamp"].as_u64().unwrap();
            assert!(
                ts > last_ts,
                "samples must be strictly increasing across pushes"
            );
            last_ts = ts;
            assert_eq!(s["host"]["name"], "test-host");
            assert_eq!(s["services"].as_array().unwrap().len(), 3);
        }
        total += samples.len();
    }
    assert!(total >= 3);
}

#[test]
fn buffers_through_an_outage_then_catches_up_and_alerts() {
    let discord = Stub::start();
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let extra = "alert_after = \"600ms\"\n[discord]\nenabled = true\nstartup_message = false";
    let _app = App::start_full(
        "100ms",
        "1h",
        &push_config(&url, extra),
        secrets(Some(&discord)),
    );

    // Nothing is listening, so pushes fail and the alert fires.
    let firing = discord.wait_for_content("ingest unreachable");
    assert!(
        firing.starts_with("**FIRING** [test-host] ingest unreachable: no successful push for"),
        "{firing}"
    );
    assert!(firing.contains("samples buffered)"), "{firing}");

    // The server comes up on the port the agent already knows.
    let ingest = Stub::start_at(&format!("127.0.0.1:{port}"));
    let first = wait_until("catch-up push", || ingest.requests().first().cloned());
    let body: Value = serde_json::from_str(&first.body).unwrap();
    let caught_up = body["samples"].as_array().unwrap().len();
    assert!(
        caught_up >= 5,
        "expected a backlog, got {caught_up} samples"
    );

    let resolved = discord.wait_for_content("**RESOLVED** [test-host] ingest unreachable");
    assert!(resolved.contains("samples caught up"), "{resolved}");

    // And pushing continues at the normal cadence afterwards.
    wait_until("steady pushes", || {
        (ingest.requests().len() >= 3).then_some(())
    });
}

#[test]
fn alert_events_ride_along_and_bad_batches_are_dropped() {
    let ingest = Stub::start();
    // First reply refuses the batch; the agent must drop it and move on.
    ingest.reply_next(400, r#"{"error":"nope"}"#);
    let extra = r#"
        [[alert]]
        name = "site down"
        scope = "service:site"
        metric = "down"
    "#;
    let _app = App::start_full(
        "100ms",
        "1h",
        &push_config(&ingest.url, extra),
        secrets(None),
    );

    // `site` has no cgroup, so its down rule fires on the first sample and
    // the event travels in the same request as the samples.
    let with_event = wait_until("a push carrying the event", || {
        ingest.requests().into_iter().find(|r| {
            serde_json::from_str::<Value>(&r.body)
                .map(|b| !b["events"].as_array().unwrap().is_empty())
                .unwrap_or(false)
        })
    });
    let body: Value = serde_json::from_str(&with_event.body).unwrap();
    let event = &body["events"][0];
    assert_eq!(event["rule"], "site down");
    assert_eq!(event["kind"], "firing");
    assert_eq!(event["metric"], "down");

    // After the 400 the next request must not contain the refused samples.
    wait_until("a push after the rejected one", || {
        (ingest.requests().len() >= 3).then_some(())
    });
    let requests = ingest.requests();
    let first: Value = serde_json::from_str(&requests[0].body).unwrap();
    let second: Value = serde_json::from_str(&requests[1].body).unwrap();
    let first_last = first["samples"].as_array().unwrap().last().unwrap()["timestamp"]
        .as_u64()
        .unwrap();
    let second_first = second["samples"].as_array().unwrap()[0]["timestamp"]
        .as_u64()
        .unwrap();
    assert!(
        second_first > first_last,
        "rejected samples were sent again"
    );
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn missing_token_runs_without_pushing() {
    let ingest = Stub::start();
    let app = App::start_full(
        "100ms",
        "1h",
        &push_config(&ingest.url, ""),
        Secrets::default(),
    );
    app.wait_for_stats("a sample", |_| true);
    std::thread::sleep(Duration::from_millis(300));
    assert!(ingest.requests().is_empty());
}
