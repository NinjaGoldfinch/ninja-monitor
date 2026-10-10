//! Shared harness: runs the whole app against a temporary copy of the
//! fixture trees, plus a tiny HTTP stub that records what it receives.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;

use ninja_monitor::config::{Config, Secrets};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

pub struct App {
    pub base: String,
    pub root: TempDir,
    agent: ureq::Agent,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl App {
    pub fn start(interval: &str) -> App {
        Self::start_with(interval, "24h")
    }

    pub fn start_with(interval: &str, history: &str) -> App {
        Self::start_full(interval, history, "", Secrets::default())
    }

    /// `extra` is appended to the generated config, for rules and settings.
    pub fn start_full(interval: &str, history: &str, extra: &str, secrets: Secrets) -> App {
        let root = TempDir::new().unwrap();
        copy_dir(Path::new(FIXTURES), root.path());
        fs::create_dir(root.path().join("data")).unwrap();
        fs::write(root.path().join("data/db"), vec![1u8; 300_000]).unwrap();
        let config = Config::parse(&format!(
            r#"
            [sampling]
            interval = "{interval}"
            history = "{history}"
            disk_interval = "100ms"
            [paths]
            proc_root = "{root}/proc"
            cgroup_root = "{root}/cgroup"
            [host]
            name = "test-host"
            mounts = ["{root}"]
            [[service]]
            name = "cloudflared"
            unit = "cloudflared.service"
            [[service]]
            name = "riot-proxy"
            unit = "riot-proxy.service"
            paths = ["{root}/data"]
            [[service]]
            name = "site"
            unit = "site.service"
            {extra}
            "#,
            root = root.path().display(),
        ))
        .unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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

        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .new_agent();
        App {
            base,
            root,
            agent,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    pub fn get(&self, path: &str) -> (u16, Value) {
        let mut response = self
            .agent
            .get(format!("{}{path}", self.base))
            .call()
            .unwrap();
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&body).unwrap())
    }

    /// Polls `/stats` until `check` passes, returning the matching body.
    pub fn wait_for_stats(&self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = Value::Null;
        while Instant::now() < deadline {
            let (status, body) = self.get("/stats");
            if status == 200 && check(&body) {
                return body;
            }
            last = body;
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}; last /stats was {last:#}");
    }

    pub fn path(&self, rel: &str) -> std::path::PathBuf {
        self.root.path().join(rel)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn copy_dir(from: &Path, to: &Path) {
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

/// Write then rename, so the sampler never sees a half-written file.
pub fn write_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp-write");
    fs::write(&tmp, contents).unwrap();
    fs::rename(tmp, path).unwrap();
}

pub fn service<'a>(stats: &'a Value, name: &str) -> &'a Value {
    stats["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
}

/// Polls `check` until it returns `Some`, failing after 10 seconds.
pub fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(value) = check() {
            return value;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// Minimal HTTP/1.1 server standing in for Discord or a heartbeat checker.
/// Replies with queued responses in order, then `204 No Content`.
pub struct Stub {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    replies: Arc<Mutex<VecDeque<(u16, String)>>>,
}

impl Stub {
    pub fn start() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        let (reqs, reps) = (requests.clone(), replies.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                reqs.lock().unwrap().push(request);
                let (status, body) = reps
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((204, String::new()));
                let reply = format!(
                    "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Stub {
            url,
            requests,
            replies,
        }
    }

    pub fn reply_next(&self, status: u16, body: &str) {
        self.replies
            .lock()
            .unwrap()
            .push_back((status, body.to_string()));
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Discord message texts received so far.
    pub fn contents(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|r| serde_json::from_str::<Value>(&r.body).ok())
            .filter_map(|v| v["content"].as_str().map(str::to_string))
            .collect()
    }

    pub fn wait_for_content(&self, needle: &str) -> String {
        wait_until(&format!("a message containing {needle:?}"), || {
            self.contents().into_iter().find(|c| c.contains(needle))
        })
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}
