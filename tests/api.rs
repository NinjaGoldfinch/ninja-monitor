//! HTTP API tests against the fixture trees.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use common::{App, service, write_atomic};

#[test]
fn stats_match_fixture_tree() {
    // A long interval means only the immediate first tick happens.
    let app = App::start("1h");
    let stats = app.wait_for_stats("first sample", |_| true);

    assert!(stats["timestamp"].as_u64().unwrap() > 1_700_000_000_000);
    let host = &stats["host"];
    assert_eq!(host["name"], "test-host");
    assert_eq!(host["cpu_percent"], Value::Null);
    assert_eq!(host["cores"], 2);
    assert_eq!(host["load1"], 0.33);
    assert_eq!(host["load5"], 0.27);
    assert_eq!(host["load15"], 0.26);
    assert_eq!(host["mem_total_bytes"], 3906236u64 * 1024);
    assert_eq!(host["mem_available_bytes"], 3167336u64 * 1024);
    assert_eq!(host["mem_used_bytes"], (3906236u64 - 3167336) * 1024);
    assert_eq!(
        host["mem_cache_bytes"],
        (2359700u64 + 200412 + 304020) * 1024
    );
    assert_eq!(host["swap_total_bytes"], 0);
    assert_eq!(host["swap_used_bytes"], 0);
    assert_eq!(host["uptime_secs"], 18486);
    let disk = &host["disks"][0];
    assert_eq!(disk["mount"], app.root.path().display().to_string());
    assert!(disk["total_bytes"].as_u64().unwrap() > 0);
    assert!(disk["used_percent"].as_f64().is_some());

    let cf = service(&stats, "cloudflared");
    assert_eq!(cf["unit"], "cloudflared.service");
    assert_eq!(cf["state"], "running");
    assert_eq!(cf["cpu_percent"], Value::Null);
    assert_eq!(cf["mem_current_bytes"], 59006976);
    assert_eq!(cf["mem_anon_bytes"], 16326656);
    assert_eq!(cf["mem_file_bytes"], 41074688);
    assert_eq!(cf["mem_peak_bytes"], 64307200);
    assert_eq!(cf["oom_kills"], 0);
    assert_eq!(cf["pids"], 8);
    assert_eq!(cf["restarts_seen"], 0);
    assert_eq!(cf["disk_bytes"], Value::Null);
    assert_eq!(cf["disk_measured_at"], Value::Null);

    let riot = service(&stats, "riot-proxy");
    assert_eq!(riot["state"], "running");
    assert_eq!(riot["mem_anon_bytes"], 208506880);
    assert_eq!(riot["mem_file_bytes"], 403083264);
    assert_eq!(riot["mem_peak_bytes"], Value::Null);

    let site = service(&stats, "site");
    assert_eq!(site["state"], "down");
    assert_eq!(site["mem_current_bytes"], Value::Null);
    assert_eq!(site["cpu_percent"], Value::Null);
}

#[test]
fn stats_follow_changes_between_ticks() {
    let app = App::start("250ms");
    app.wait_for_stats("first sample", |_| true);

    // Host: 100 more busy and 100 more idle jiffies is 50 %.
    let stat_path = app.path("proc/stat");
    let stat = fs::read_to_string(&stat_path).unwrap().replacen(
        "cpu  531316 0 82729 3055641",
        "cpu  531416 0 82729 3055741",
        1,
    );
    write_atomic(&stat_path, &stat);

    // Service: more CPU time and more memory.
    let cf = app.path("cgroup/system.slice/cloudflared.service");
    let cpu = fs::read_to_string(cf.join("cpu.stat")).unwrap();
    write_atomic(
        &cf.join("cpu.stat"),
        &cpu.replace("usage_usec 54669841", "usage_usec 54769841"),
    );
    write_atomic(&cf.join("memory.current"), "70000000\n");
    let events = fs::read_to_string(cf.join("memory.events")).unwrap();
    write_atomic(
        &cf.join("memory.events"),
        &events.replace("oom_kill 0", "oom_kill 1"),
    );

    let stats = app.wait_for_stats("host and service CPU to move", |s| {
        s["host"]["cpu_percent"] == 50.0
            && service(s, "cloudflared")["cpu_percent"]
                .as_f64()
                .is_some_and(|p| p > 0.0)
    });
    let cf_stats = service(&stats, "cloudflared");
    assert_eq!(cf_stats["mem_current_bytes"], 70000000);
    assert_eq!(cf_stats["oom_kills"], 1);

    // Stop the service: systemd removes its cgroup.
    let riot = app.path("cgroup/system.slice/riot-proxy.service");
    let saved = app.path("riot-saved");
    fs::rename(&riot, &saved).unwrap();
    let stats = app.wait_for_stats("riot-proxy down", |s| {
        service(s, "riot-proxy")["state"] == "down"
    });
    let riot_stats = service(&stats, "riot-proxy");
    assert_eq!(riot_stats["mem_anon_bytes"], Value::Null);
    assert_eq!(riot_stats["pids"], Value::Null);

    // It comes back with a fresh, lower CPU counter.
    let cpu = fs::read_to_string(saved.join("cpu.stat")).unwrap();
    write_atomic(
        &saved.join("cpu.stat"),
        &cpu.replace("usage_usec 4591384217", "usage_usec 1000"),
    );
    fs::rename(&saved, &riot).unwrap();
    let stats = app.wait_for_stats("riot-proxy restart", |s| {
        service(s, "riot-proxy")["restarts_seen"] == 1
    });
    assert_eq!(service(&stats, "riot-proxy")["state"], "running");

    // A service whose pids drop to zero is down even though its cgroup exists.
    write_atomic(&cf.join("pids.current"), "0\n");
    app.wait_for_stats("cloudflared down", |s| {
        service(s, "cloudflared")["state"] == "down"
    });
}

#[test]
fn malformed_files_become_null() {
    let app = App::start("250ms");
    app.wait_for_stats("first sample", |_| true);

    write_atomic(&app.path("proc/meminfo"), "not meminfo at all\n");
    write_atomic(&app.path("proc/loadavg"), "garbage\n");
    let cf = app.path("cgroup/system.slice/cloudflared.service");
    write_atomic(&cf.join("memory.current"), "max\n");

    let stats = app.wait_for_stats("nulls", |s| s["host"]["mem_total_bytes"].is_null());
    assert_eq!(stats["host"]["load1"], Value::Null);
    assert_eq!(stats["host"]["cores"], 2);
    let cf_stats = service(&stats, "cloudflared");
    assert_eq!(cf_stats["mem_current_bytes"], Value::Null);
    assert_eq!(cf_stats["mem_anon_bytes"], 16326656);
}

#[test]
fn healthz_and_unknown_routes() {
    let app = App::start("1h");
    let (status, body) = app.get("/healthz");
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["uptime_secs"].as_u64().is_some());

    let (status, body) = app.get("/nope");
    assert_eq!(status, 404);
    assert_eq!(body["error"], "not found");
}

fn column_len(body: &Value, key: &str) -> usize {
    body[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} missing"))
        .len()
}

#[test]
fn history_returns_columns_for_host_and_services() {
    let app = App::start("100ms");
    // Wait for several samples.
    thread::sleep(Duration::from_millis(700));

    let (status, body) = app.get("/history?scope=host&minutes=5");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["scope"], "host");
    assert_eq!(body["minutes"], 5);
    assert_eq!(body["bucket"], 1);
    let n = column_len(&body, "t");
    assert!(n >= 4, "only {n} host points");
    for key in [
        "cpu_percent",
        "load1",
        "mem_available_bytes",
        "mem_used_bytes",
        "swap_used_bytes",
    ] {
        assert_eq!(column_len(&body, key), n, "{key}");
    }
    let to = body["to"].as_u64().unwrap();
    assert_eq!(body["from"].as_u64().unwrap(), to - 5 * 60_000);
    let t: Vec<u64> = body["t"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert!(t.windows(2).all(|w| w[0] < w[1]), "times not increasing");
    assert!(t.iter().all(|&x| x <= to && x >= to - 5 * 60_000));
    assert_eq!(
        body["cpu_percent"][0],
        Value::Null,
        "first sample has no CPU"
    );
    assert_eq!(body["mem_available_bytes"][0], 3167336u64 * 1024);
    assert_eq!(
        body["disks"][0]["mount"],
        app.root.path().display().to_string()
    );
    assert_eq!(column_len(&body["disks"][0], "used_percent"), n);

    let (status, body) = app.get("/history?scope=service:cloudflared");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["scope"], "service:cloudflared");
    assert_eq!(body["minutes"], 60);
    let n = column_len(&body, "t");
    assert!(n >= 4);
    assert_eq!(body["up"][0], 1.0);
    assert_eq!(body["mem_anon_bytes"][0], 16326656);
    assert_eq!(body["pids"][0], 8);
    assert_eq!(column_len(&body, "oom_kills"), n);

    let (_, body) = app.get("/history?scope=service:site");
    assert!(body["up"].as_array().unwrap().iter().all(|v| v == 0.0));
    assert!(
        body["mem_current_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_null)
    );
}

#[test]
fn history_downsamples_to_max_points() {
    let app = App::start("100ms");
    thread::sleep(Duration::from_millis(1000));

    let (_, full) = app.get("/history?scope=service:riot-proxy");
    let n = column_len(&full, "t");
    assert!(n >= 6, "only {n} points");

    let (status, body) = app.get("/history?scope=service:riot-proxy&max_points=3");
    assert_eq!(status, 200);
    let points = column_len(&body, "t");
    assert!((1..=3).contains(&points), "{points} points");
    let bucket = body["bucket"].as_u64().unwrap() as usize;
    assert!(bucket >= 2);
    assert_eq!(column_len(&body, "mem_anon_bytes"), points);
    // Constant fixture values average to themselves.
    assert_eq!(body["mem_anon_bytes"][0], 208506880);

    let (_, one) = app.get("/history?scope=host&max_points=1");
    assert_eq!(column_len(&one, "t"), 1);
    assert_eq!(column_len(&one["disks"][0], "used_bytes"), 1);
}

#[test]
fn history_minutes_capped_at_history_length() {
    let app = App::start_with("100ms", "2m");
    let (status, body) = app.get("/history?scope=host&minutes=100000");
    assert_eq!(status, 200);
    assert_eq!(body["minutes"], 2);
}

#[test]
fn history_bad_requests() {
    let app = App::start("1h");
    for (path, status) in [
        ("/history", 400),
        ("/history?scope=host&minutes=0", 400),
        ("/history?scope=host&minutes=-5", 400),
        ("/history?scope=host&minutes=abc", 400),
        ("/history?scope=host&max_points=0", 400),
        ("/history?scope=host&max_points=1.5", 400),
        ("/history?scope=service:nope", 404),
        ("/history?scope=galaxy", 404),
        ("/history?scope=service:", 404),
    ] {
        let (got, body) = app.get(path);
        assert_eq!(got, status, "{path}: {body}");
        assert!(body["error"].as_str().is_some(), "{path}: {body}");
    }
}

#[test]
fn disk_usage_reported_with_timestamp() {
    let app = App::start("100ms");
    let stats = app.wait_for_stats("disk usage", |s| {
        service(s, "riot-proxy")["disk_bytes"].as_u64().is_some()
    });
    let riot = service(&stats, "riot-proxy");
    let bytes = riot["disk_bytes"].as_u64().unwrap();
    assert!(bytes >= 300_000, "{bytes}");
    let measured = riot["disk_measured_at"].as_u64().unwrap();
    assert!(measured <= stats["timestamp"].as_u64().unwrap() + 1000);
    // Services without paths never get a figure.
    assert_eq!(service(&stats, "cloudflared")["disk_bytes"], Value::Null);

    // Growth shows up on a later walk.
    fs::write(app.path("data/more"), vec![1u8; 1_000_000]).unwrap();
    app.wait_for_stats("disk usage to grow", |s| {
        service(s, "riot-proxy")["disk_bytes"]
            .as_u64()
            .is_some_and(|b| b >= bytes + 1_000_000)
    });

    let (_, history) = app.get("/history?scope=service:riot-proxy");
    assert!(
        history["disk_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_u64().is_some_and(|b| b >= 300_000))
    );
}
