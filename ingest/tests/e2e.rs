//! A real ninja-monitor agent, sampling the fixture trees, pushing to a real
//! ninja-ingest server. The server goes away for a while and comes back.

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;

use common::{Server, TOKEN_VPS, get_json};
use ninja_monitor::config::{Config as AgentConfig, Secrets as AgentSecrets};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures");
const INTERVAL: Duration = Duration::from_millis(200);

struct Agent {
    base: String,
    _root: TempDir,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Agent {
    fn start(ingest_base: &str) -> Agent {
        let root = TempDir::new().unwrap();
        copy_dir(Path::new(FIXTURES), root.path());
        let config = AgentConfig::parse(&format!(
            r#"
            [sampling]
            interval = "200ms"
            disk_interval = "1h"
            history = "1h"
            [paths]
            proc_root = "{root}/proc"
            cgroup_root = "{root}/cgroup"
            [host]
            name = "vps"
            mounts = ["{root}"]
            [[service]]
            name = "cloudflared"
            unit = "cloudflared.service"
            [[service]]
            name = "riot-proxy"
            unit = "riot-proxy.service"
            [push]
            enabled = true
            url = "http://{ingest_base}/v1/ingest"
            flush_interval = "200ms"
            buffer = "1m"
            alert_after = "0s"
            "#,
            root = root.path().display(),
        ))
        .unwrap();
        let secrets = AgentSecrets {
            push_token: Some(TOKEN_VPS.into()),
            ..AgentSecrets::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = listener.local_addr().unwrap().to_string();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                ninja_monitor::run(config, secrets, listener, async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
        });
        Agent {
            base,
            _root: root,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn history_times(&self) -> Vec<u64> {
        let (status, body) = get_json(&self.base, "/history?scope=host&minutes=60");
        assert_eq!(status, 200, "{body}");
        times(&body)
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn times(history: &Value) -> Vec<u64> {
    history["t"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect()
}

fn server_times(server: &Server) -> Vec<u64> {
    let (status, body) = server.get("/hosts/vps/history?scope=host&minutes=60&max_points=2000");
    assert_eq!(status, 200, "{body}");
    times(&body)
}

fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(value) = check() {
            return value;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn agent_pushes_and_catches_up_after_an_outage() {
    let server = Server::start("");
    let agent = Agent::start(&server.base);

    // Steady state: the server has what the agent has, give or take the
    // sample currently in flight.
    wait_until("five samples on the server", || {
        (server_times(&server).len() >= 5).then_some(())
    });
    let on_server = server_times(&server);
    let on_agent = agent.history_times();
    assert!(
        on_agent.starts_with(&on_server),
        "server history must be a prefix of the agent's:\n{on_server:?}\n{on_agent:?}"
    );
    let (status, stats) = server.get("/hosts/vps/stats");
    assert_eq!(status, 200, "{stats}");
    assert_eq!(stats["host"]["name"], "vps");
    assert_eq!(stats["services"].as_array().unwrap().len(), 2);
    assert_eq!(
        stats["services"][0]["name"], "cloudflared",
        "agent order is kept"
    );

    // Outage: the server stops, the agent keeps sampling and buffering.
    let before = on_server.len();
    let mut server = server;
    server.shutdown();
    thread::sleep(INTERVAL * 8);
    let buffered_until = agent.history_times();
    assert!(buffered_until.len() >= before + 5, "agent kept sampling");

    // Back on the same port: the backlog arrives and the gap is filled.
    let server = server.restart_same_port("");
    let caught_up = wait_until("the backlog to arrive", || {
        let t = server_times(&server);
        (t.len() >= buffered_until.len()).then_some(t)
    });
    assert!(
        agent.history_times().starts_with(&caught_up),
        "server history must still be a prefix of the agent's"
    );
    for pair in caught_up.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap < INTERVAL.as_millis() as u64 * 4,
            "a hole of {gap} ms is left in the server's history: {caught_up:?}"
        );
    }

    // Nothing was stored twice.
    let (_, health) = server.get("/healthz");
    assert_eq!(health["hosts"], 1);
    let (_, hosts) = server.get("/hosts");
    let vps = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap();
    assert_eq!(vps["status"], "reporting");
    assert!(
        vps["agent"].as_str().unwrap().starts_with("ninja-monitor/"),
        "{vps}"
    );
}
