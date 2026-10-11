//! Silence detection, its persistence across a restart, and the heartbeat,
//! with stubs standing in for Discord and the external checker.

mod common;

use std::thread;
use std::time::Duration;

use common::{Outbound, Server, Stub, TOKEN_VPS, now_ms, sample, wait_until};

fn discord(stub: &Stub) -> Outbound {
    Outbound {
        discord: Some(format!("{}/api/webhooks/1/token", stub.url)),
        heartbeat: None,
    }
}

const FAST: &str = "silence.after = \"300ms\"\ndiscord.enabled = true";

#[test]
fn silent_then_reporting_with_persistence_across_restart() {
    let stub = Stub::start();
    let server = Server::start_with(FAST, discord(&stub));
    let started = stub.wait_for_content("STARTED");
    assert!(
        started.contains("[ninja-ingest]") && started.contains("2 hosts"),
        "{started}"
    );

    // Never-seen hosts are not silent, however long we wait.
    thread::sleep(Duration::from_millis(700));
    assert_eq!(stub.contents().len(), 1, "{:?}", stub.contents());

    server.push(TOKEN_VPS, "vps", &[sample("vps", now_ms(), 0)], &[]);
    let silent = stub.wait_for_content("**SILENT** [vps]");
    assert!(
        silent.contains("no samples for") && silent.contains("UTC"),
        "{silent}"
    );
    let (_, hosts) = server.get("/hosts");
    let vps = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(vps["status"], "silent");
    let persisted = vps["silent_since"].as_u64().unwrap();
    assert_eq!(persisted, vps["last_seen"].as_u64().unwrap() + 300);

    // Restart while silent: nothing is announced twice, silent_since survives.
    let stub2 = Stub::start();
    let server = server.restart_with(FAST, discord(&stub2));
    stub2.wait_for_content("STARTED");
    thread::sleep(Duration::from_millis(700));
    assert!(
        stub2.contents().iter().all(|c| !c.contains("SILENT")),
        "{:?}",
        stub2.contents()
    );
    let (_, hosts) = server.get("/hosts");
    let vps = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(vps["silent_since"], persisted);

    // A sample arrives: REPORTING, and the flag clears.
    server.push(TOKEN_VPS, "vps", &[sample("vps", now_ms(), 1)], &[]);
    let back = stub2.wait_for_content("**REPORTING** [vps] back after");
    assert!(!back.contains("SILENT"), "{back}");
    let (_, hosts) = server.get("/hosts");
    let vps = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(vps["status"], "reporting", "{vps}");
    assert!(vps["silent_since"].is_null());

    // And it goes silent again afterwards.
    let second = wait_until("a second SILENT", || {
        let n = stub2
            .contents()
            .iter()
            .filter(|c| c.contains("**SILENT**"))
            .count();
        (n >= 1).then_some(n)
    });
    assert_eq!(second, 1);
}

#[test]
fn repeat_reminds_while_silent() {
    let stub = Stub::start();
    let extra = "silence.after = \"200ms\"\nsilence.repeat = \"300ms\"\ndiscord.enabled = true\ndiscord.startup_message = false";
    let server = Server::start_with(extra, discord(&stub));
    server.push(TOKEN_VPS, "vps", &[sample("vps", now_ms(), 0)], &[]);
    wait_until("three SILENT messages", || {
        let silent: Vec<_> = stub
            .contents()
            .into_iter()
            .filter(|c| c.starts_with("**SILENT** [vps]"))
            .collect();
        (silent.len() >= 3).then_some(silent)
    });
    let messages = stub.contents();
    assert!(messages[0].contains("no samples for"), "{messages:?}");
    assert!(
        messages[1].contains("still no samples after"),
        "{messages:?}"
    );
}

#[test]
fn went_silent_while_the_server_was_down_is_reported_at_start() {
    let server = Server::start("silence.after = \"200ms\"");
    server.push(TOKEN_VPS, "vps", &[sample("vps", now_ms(), 0)], &[]);
    let mut server = server;
    server.shutdown();
    thread::sleep(Duration::from_millis(400));

    let stub = Stub::start();
    let server = server.restart_with(FAST, discord(&stub));
    let silent = stub.wait_for_content("**SILENT** [vps]");
    assert!(silent.contains("no samples for"), "{silent}");
    drop(server);
}

#[test]
fn heartbeat_pings_when_the_database_answers() {
    let checker = Stub::start();
    let outbound = Outbound {
        discord: None,
        heartbeat: Some(format!("{}/ping/abc", checker.url)),
    };
    let _server = Server::start_with(
        "heartbeat.enabled = true\nheartbeat.interval = \"100ms\"",
        outbound,
    );
    wait_until("three heartbeats", || {
        (checker.requests().len() >= 3).then_some(())
    });
    for request in checker.requests() {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/ping/abc");
    }
}
