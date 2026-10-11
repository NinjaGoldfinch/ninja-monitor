//! SQLite storage. One connection behind a mutex; every call runs on the
//! blocking pool so the HTTP handlers never wait on disk.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags};

/// Schema versions, applied in order. Never edit an entry that has shipped;
/// append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "
    CREATE TABLE host (
        id          INTEGER PRIMARY KEY,
        name        TEXT NOT NULL UNIQUE,
        first_seen  INTEGER NOT NULL,
        last_seen   INTEGER NOT NULL,
        silent_since INTEGER,
        agent       TEXT
    );

    CREATE TABLE host_sample (
        host_id INTEGER NOT NULL REFERENCES host(id),
        ts      INTEGER NOT NULL,
        cpu_percent REAL, cores INTEGER, load1 REAL, load5 REAL, load15 REAL,
        mem_total_bytes INTEGER, mem_available_bytes INTEGER, mem_used_bytes INTEGER,
        mem_cache_bytes INTEGER, swap_total_bytes INTEGER, swap_used_bytes INTEGER,
        uptime_secs INTEGER,
        PRIMARY KEY (host_id, ts)
    ) WITHOUT ROWID;

    CREATE TABLE disk_sample (
        host_id INTEGER NOT NULL REFERENCES host(id),
        ts      INTEGER NOT NULL,
        mount   TEXT NOT NULL,
        total_bytes INTEGER, used_bytes INTEGER, avail_bytes INTEGER, used_percent REAL,
        PRIMARY KEY (host_id, mount, ts)
    ) WITHOUT ROWID;

    CREATE TABLE service_sample (
        host_id INTEGER NOT NULL REFERENCES host(id),
        ts      INTEGER NOT NULL,
        service TEXT NOT NULL,
        unit    TEXT,
        up      INTEGER NOT NULL,
        cpu_percent REAL, mem_current_bytes INTEGER, mem_anon_bytes INTEGER,
        mem_file_bytes INTEGER, mem_peak_bytes INTEGER, oom_kills INTEGER, pids INTEGER,
        restarts_seen INTEGER, disk_bytes INTEGER, disk_measured_at INTEGER,
        PRIMARY KEY (host_id, service, ts)
    ) WITHOUT ROWID;

    CREATE TABLE alert_event (
        host_id INTEGER NOT NULL REFERENCES host(id),
        ts      INTEGER NOT NULL,
        rule    TEXT NOT NULL,
        scope   TEXT NOT NULL,
        metric  TEXT NOT NULL,
        kind    TEXT NOT NULL,
        value REAL, op TEXT, threshold REAL, held_secs INTEGER,
        PRIMARY KEY (host_id, ts, rule)
    ) WITHOUT ROWID;
    ",
];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Opens or creates the database, sets the pragmas and applies any
    /// pending migrations. The parent directory must already exist.
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Opens an existing database read-write without creating it. Used by
    /// `--check`, which must not leave files behind.
    pub fn open_existing(path: &Path) -> anyhow::Result<Db> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::from_connection(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> anyhow::Result<Db> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> anyhow::Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("cannot enable WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` with the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        })
        .await
        .context("database task panicked")?
        .context("database query failed")
    }

    /// Same as `call`, on the current thread. For startup, `--check` and tests.
    pub fn call_sync<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T>,
    {
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard).context("database query failed")
    }

    pub fn schema_version_sync(&self) -> anyhow::Result<u32> {
        self.call_sync(schema_version)
    }

    pub async fn host_count(&self) -> anyhow::Result<u64> {
        self.call(|c| c.query_row("SELECT count(*) FROM host", [], |r| r.get::<_, i64>(0)))
            .await
            .map(|n| n.max(0) as u64)
    }

    /// Pages in use times the page size: the logical size, ignoring the WAL.
    pub async fn size_bytes(&self) -> anyhow::Result<u64> {
        self.call(|c| {
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            Ok((pages.max(0) as u64) * (size.max(0) as u64))
        })
        .await
    }
}

fn schema_version(conn: &mut Connection) -> rusqlite::Result<u32> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(version.and_then(|v| v.parse().ok()).unwrap_or(0))
}

fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let current = schema_version(conn)?;
    if current > SCHEMA_VERSION {
        anyhow::bail!(
            "database schema version {current} is newer than this binary supports ({SCHEMA_VERSION}); upgrade ninja-ingest"
        );
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as u32 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("schema migration {version} failed"))?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [version.to_string()],
        )?;
        tx.commit()?;
        tracing::info!(version, "applied schema migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_schema_on_fresh_database() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.schema_version_sync().unwrap(), SCHEMA_VERSION);
        let tables: Vec<String> = db
            .call_sync(|c| {
                let mut stmt =
                    c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect()
            })
            .unwrap();
        assert_eq!(
            tables,
            [
                "alert_event",
                "disk_sample",
                "host",
                "host_sample",
                "meta",
                "service_sample"
            ]
        );
    }

    #[test]
    fn reopening_is_a_no_op_and_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ingest.db");
        {
            let db = Db::open(&path).unwrap();
            assert_eq!(db.schema_version_sync().unwrap(), SCHEMA_VERSION);
        }
        {
            let db = Db::open_existing(&path).unwrap();
            assert_eq!(db.schema_version_sync().unwrap(), SCHEMA_VERSION);
            db.call_sync(|c| {
                c.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    [(SCHEMA_VERSION + 1).to_string()],
                )
            })
            .unwrap();
        }
        let err = Db::open(&path).err().expect("newer schema must be refused");
        assert!(format!("{err:#}").contains("newer than this binary"));
        assert!(Db::open_existing(&dir.path().join("missing.db")).is_err());
    }

    #[tokio::test]
    async fn async_calls_work() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.host_count().await.unwrap(), 0);
        assert!(db.size_bytes().await.unwrap() > 0);
    }
}
