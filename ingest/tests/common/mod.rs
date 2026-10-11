//! Shared harness: runs the server on an ephemeral port against a temporary
//! database, with a tiny HTTP/1.1 client that needs no extra crates.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener as StdListener, TcpStream};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;

use ninja_ingest::config::{Config, Secrets};
use ninja_ingest::db::Db;
use ninja_monitor::alerts::{AlertEvent, EventKind};
use ninja_monitor::sample::{DiskSample, HostSample, Sample, ServiceSample, ServiceState};

pub const TOKEN_VPS: &str = "vps-token-0123456789abcdef0123456789abcdef";
pub const TOKEN_HOME: &str = "home-token-0123456789abcdef0123456789abcde";

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Server {
    pub base: String,
    pub db_path: PathBuf,
    pub dir: TempDir,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts with two configured agents, `vps` and `home`, and `extra`
    /// (dotted TOML keys) added to the config.
    pub fn start(extra: &str) -> Server {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("ingest.db");
        Self::start_at(dir, db_path, extra)
    }

    /// Starts again on an existing database file, as a restart would.
    pub fn restart(mut self, extra: &str) -> Server {
        self.shutdown();
        let dir = std::mem::replace(&mut self.dir, TempDir::new().unwrap());
        let db_path = self.db_path.clone();
        Self::start_at(dir, db_path, extra)
    }

    /// Starts on an existing database in `dir`.
    pub fn start_on(dir: TempDir, db_path: PathBuf, extra: &str) -> Server {
        Self::start_at(dir, db_path, extra)
    }

    fn start_at(dir: TempDir, db_path: PathBuf, extra: &str) -> Server {
        // `extra` uses dotted keys (`silence.after = "1m"`) so it can set any
        // section without clashing with the ones written here.
        let config = Config::parse(&format!(
            r#"
            server.bind = "127.0.0.1:0"
            storage.path = "{}"
            {extra}
            [[agent]]
            name = "vps"
            token_env = "NINJA_INGEST_TEST_TOKEN_VPS"
            [[agent]]
            name = "home"
            token_env = "NINJA_INGEST_TEST_TOKEN_HOME"
            "#,
            db_path.display()
        ))
        .unwrap();
        let secrets = Secrets {
            tokens: vec![
                ("vps".into(), TOKEN_VPS.into()),
                ("home".into(), TOKEN_HOME.into()),
            ],
            ..Secrets::default()
        };
        let db = Db::open(&db_path).unwrap();

        let std_listener = StdListener::bind(config.server.bind).unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let base = std_listener.local_addr().unwrap().to_string();

        let (stop, stopped) = oneshot::channel::<()>();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                ninja_ingest::run(config, secrets, db, listener, async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
        });
        Server {
            base,
            db_path,
            dir,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Response {
        let mut stream = TcpStream::connect(&self.base).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.base
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if body.is_some() {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        // A server that rejects on the headers alone may close before the
        // body is written; the response is still worth reading.
        let _ = stream.write_all(payload.as_bytes());

        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("http response");
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
        };
        Response {
            status,
            headers,
            body,
        }
    }

    pub fn get(&self, path: &str) -> (u16, Value) {
        let r = self.request("GET", path, &[], None);
        (r.status, r.body)
    }

    pub fn post(&self, token: Option<&str>, body: &Value) -> (u16, Value) {
        let auth = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> =
            auth.iter().map(|a| ("Authorization", a.as_str())).collect();
        let r = self.request("POST", "/v1/ingest", &headers, Some(body));
        (r.status, r.body)
    }

    /// Pushes samples and events as `host`, asserting success.
    pub fn push(
        &self,
        token: &str,
        host: &str,
        samples: &[Sample],
        events: &[AlertEvent],
    ) -> Value {
        let body = serde_json::json!({
            "agent": "ninja-monitor/0.1.0-test",
            "host": host,
            "samples": samples,
            "events": events,
        });
        let (status, body) = self.post(Some(token), &body);
        assert_eq!(status, 200, "{body}");
        body
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn now_ms() -> u64 {
    ninja_monitor::sampler::unix_millis()
}

/// A realistic sample for host `name` at `t`, varying with `i`.
pub fn sample(name: &str, t: u64, i: u64) -> Sample {
    let running = i % 5 != 3;
    Sample {
        timestamp: t,
        host: HostSample {
            name: name.into(),
            cpu_percent: (i > 0).then_some(10.0 + i as f64),
            cores: Some(2),
            load1: Some(0.33 + i as f64 / 100.0),
            load5: None,
            load15: Some(0.1),
            mem_total_bytes: Some(4_000_000_000),
            mem_available_bytes: Some(3_000_000_000 - i * 1_000_000),
            mem_used_bytes: Some(1_000_000_000 + i * 1_000_000),
            mem_cache_bytes: i.is_multiple_of(2).then_some(500_000_000),
            swap_total_bytes: Some(0),
            swap_used_bytes: Some(0),
            uptime_secs: Some(86_400 + i),
            disks: vec![DiskSample {
                mount: "/".into(),
                total_bytes: Some(40_000_000_000),
                used_bytes: Some(10_000_000_000 + i * 1000),
                avail_bytes: Some(30_000_000_000),
                used_percent: Some(25.0 + i as f64 / 10.0),
            }],
        },
        services: vec![
            ServiceSample {
                name: "riot-proxy".into(),
                unit: "riot-proxy.service".into(),
                state: if running {
                    ServiceState::Running
                } else {
                    ServiceState::Down
                },
                cpu_percent: (running && i > 0).then_some(2.5 * i as f64),
                mem_current_bytes: running.then_some(618_397_696 + i),
                mem_anon_bytes: running.then_some(208_506_880 + i * 10),
                mem_file_bytes: running.then_some(403_083_264),
                mem_peak_bytes: None,
                oom_kills: Some(i / 7),
                pids: running.then_some(14),
                restarts_seen: i / 5,
                disk_bytes: Some(73_728),
                disk_measured_at: Some(t - 1000),
            },
            ServiceSample {
                name: "cloudflared".into(),
                unit: "cloudflared.service".into(),
                state: ServiceState::Running,
                cpu_percent: Some(0.5),
                mem_current_bytes: Some(30_000_000),
                mem_anon_bytes: Some(20_000_000),
                mem_file_bytes: Some(10_000_000),
                mem_peak_bytes: Some(35_000_000),
                oom_kills: Some(0),
                pids: Some(9),
                restarts_seen: 0,
                disk_bytes: None,
                disk_measured_at: None,
            },
        ],
    }
}

pub fn event(t: u64, rule: &str, kind: EventKind) -> AlertEvent {
    AlertEvent {
        timestamp: t,
        rule: rule.into(),
        scope: "service:riot-proxy".into(),
        metric: "mem_anon_bytes".into(),
        kind,
        value: Some(2.0e9),
        op: Some(">".into()),
        threshold: Some(1.9e9),
        held_secs: 312,
    }
}
