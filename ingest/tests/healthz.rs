//! Starts the server on an ephemeral port with a temporary database and
//! checks the one route phase 1 provides.

use std::io::{Read, Write};
use std::net::{TcpListener as StdListener, TcpStream};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;

use ninja_ingest::config::{Config, Secrets};
use ninja_ingest::db::Db;

fn get(base: &str, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(base).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {base}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("http response");
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).expect("json body"))
}

#[test]
fn healthz_reports_database_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ingest.db");
    let config = Config::parse(&format!(
        r#"
        [server]
        bind = "127.0.0.1:0"
        [storage]
        path = "{}"
        [[agent]]
        name = "vps"
        token_env = "NINJA_INGEST_TEST_TOKEN_VPS"
        "#,
        path.display()
    ))
    .unwrap();
    let db = Db::open(&path).unwrap();

    // Bind synchronously so the port is known before the server thread starts.
    let std_listener = StdListener::bind(config.server.bind).unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let base = std_listener.local_addr().unwrap().to_string();

    let (stop, stopped) = oneshot::channel::<()>();
    let server = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            ninja_ingest::run(config, Secrets::default(), db, listener, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        });
    });

    let (status, body) = get(&base, "/healthz");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["hosts"], 0);
    assert!(body["db_bytes"].as_u64().unwrap() > 0);
    assert!(body["uptime_secs"].is_u64());

    let (status, body) = get(&base, "/nope");
    assert_eq!(status, 404);
    assert_eq!(body["error"], "not found");

    stop.send(()).unwrap();
    server.join().unwrap();
    assert!(path.exists());
}
