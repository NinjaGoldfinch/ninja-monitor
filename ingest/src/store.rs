//! SQL for writing batches and reading them back. Every function here is
//! synchronous and runs inside `Db::call`.
//!
//! History windows are read into the agent's own point types and handed to
//! the agent's `history` module, so a server response is identical to what
//! the agent would have produced from the same samples.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use ninja_monitor::alerts::{AlertEvent, EventKind};
use ninja_monitor::sample::{DiskSample, HostSample, Sample, ServiceSample, ServiceState};
use ninja_monitor::store::{DiskPoint, HostPoint, ServicePoint, pack_f32, pack_u32, pack_u64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub id: i64,
    pub name: String,
    pub first_seen: u64,
    pub last_seen: u64,
    pub silent_since: Option<u64>,
    pub agent: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Inserted {
    pub accepted: usize,
    pub duplicates: usize,
    pub events: usize,
    /// The agent string stored before this batch, if the host already existed.
    pub previous_agent: Option<Option<String>>,
}

fn u(v: Option<i64>) -> Option<u64> {
    v.map(|x| x.max(0) as u64)
}

fn i(v: Option<u64>) -> Option<i64> {
    v.map(clamp)
}

/// SQLite integers are signed; anything larger saturates rather than wrapping.
fn clamp(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

fn host_row(r: &Row) -> rusqlite::Result<HostRow> {
    Ok(HostRow {
        id: r.get(0)?,
        name: r.get(1)?,
        first_seen: r.get::<_, i64>(2)?.max(0) as u64,
        last_seen: r.get::<_, i64>(3)?.max(0) as u64,
        silent_since: u(r.get(4)?),
        agent: r.get(5)?,
    })
}

const HOST_COLUMNS: &str = "id, name, first_seen, last_seen, silent_since, agent";

pub fn hosts(conn: &Connection) -> rusqlite::Result<Vec<HostRow>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {HOST_COLUMNS} FROM host ORDER BY name"))?;
    let rows = stmt.query_map([], host_row)?;
    rows.collect()
}

pub fn host_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<HostRow>> {
    conn.prepare_cached(&format!("SELECT {HOST_COLUMNS} FROM host WHERE name = ?1"))?
        .query_row([name], host_row)
        .optional()
}

pub fn set_silent_since(
    conn: &Connection,
    host_id: i64,
    value: Option<u64>,
) -> rusqlite::Result<()> {
    conn.prepare_cached("UPDATE host SET silent_since = ?2 WHERE id = ?1")?
        .execute(params![host_id, i(value)])?;
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub samples: usize,
    pub events: usize,
}

/// Deletes rows older than the retention windows, at most `batch` per
/// statement, until nothing old is left. Returns how many rows went.
pub fn sweep(
    conn: &Connection,
    now: u64,
    retention_ms: u64,
    events_retention_ms: u64,
    batch: usize,
    pause: impl Fn(),
) -> rusqlite::Result<Swept> {
    let mut out = Swept::default();
    let samples_before = clamp(now.saturating_sub(retention_ms));
    let events_before = clamp(now.saturating_sub(events_retention_ms));
    for table in ["host_sample", "disk_sample", "service_sample"] {
        out.samples += delete_old(conn, table, samples_before, batch, &pause)?;
    }
    out.events += delete_old(conn, "alert_event", events_before, batch, &pause)?;
    Ok(out)
}

fn delete_old(
    conn: &Connection,
    table: &str,
    before: i64,
    batch: usize,
    pause: &impl Fn(),
) -> rusqlite::Result<usize> {
    // Tables are WITHOUT ROWID, so delete by key range through a subquery.
    let sql = format!(
        "DELETE FROM {table} WHERE (host_id, ts) IN
         (SELECT host_id, ts FROM {table} WHERE ts < ?1 LIMIT ?2)"
    );
    let mut total = 0;
    loop {
        let deleted = conn
            .prepare_cached(&sql)?
            .execute(params![before, batch as i64])?;
        total += deleted;
        if deleted < batch {
            return Ok(total);
        }
        pause();
    }
}

/// Hands WAL pages back to the main file after a sweep.
pub fn checkpoint(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
}

/// Stores one request in one transaction. Samples already present (same
/// host and timestamp) are counted as duplicates and left untouched.
pub fn insert_batch(
    conn: &mut Connection,
    host: &str,
    agent: Option<&str>,
    received_at: u64,
    samples: &[Sample],
    events: &[AlertEvent],
) -> rusqlite::Result<Inserted> {
    let tx = conn.transaction()?;
    let previous = host_by_name(&tx, host)?;
    tx.execute(
        "INSERT INTO host (name, first_seen, last_seen, agent) VALUES (?1, ?2, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET last_seen = excluded.last_seen,
                                        agent = coalesce(excluded.agent, host.agent)",
        params![host, received_at as i64, agent],
    )?;
    let host_id: i64 = tx.query_row("SELECT id FROM host WHERE name = ?1", [host], |r| r.get(0))?;

    let mut out = Inserted {
        previous_agent: previous.map(|p| p.agent),
        ..Inserted::default()
    };
    for sample in samples {
        if insert_sample(&tx, host_id, received_at, sample)? {
            out.accepted += 1;
        } else {
            out.duplicates += 1;
        }
    }
    for event in events {
        let changed = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO alert_event
             (host_id, ts, rule, scope, metric, kind, value, op, threshold, held_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                host_id,
                event.timestamp as i64,
                event.rule,
                event.scope,
                event.metric,
                kind_name(event.kind),
                event.value,
                event.op,
                event.threshold,
                event.held_secs as i64,
            ])?;
        out.events += changed;
    }
    tx.commit()?;
    Ok(out)
}

/// Returns false if the sample was already stored.
fn insert_sample(
    tx: &Transaction,
    host_id: i64,
    received_at: u64,
    sample: &Sample,
) -> rusqlite::Result<bool> {
    let h = &sample.host;
    let ts = sample.timestamp as i64;
    let changed = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO host_sample
             (host_id, ts, received_at, cpu_percent, cores, load1, load5, load15,
              mem_total_bytes, mem_available_bytes, mem_used_bytes, mem_cache_bytes,
              swap_total_bytes, swap_used_bytes, uptime_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?
        .execute(params![
            host_id,
            ts,
            received_at as i64,
            h.cpu_percent,
            h.cores.map(i64::from),
            h.load1,
            h.load5,
            h.load15,
            i(h.mem_total_bytes),
            i(h.mem_available_bytes),
            i(h.mem_used_bytes),
            i(h.mem_cache_bytes),
            i(h.swap_total_bytes),
            i(h.swap_used_bytes),
            i(h.uptime_secs),
        ])?;
    if changed == 0 {
        return Ok(false);
    }
    for (pos, d) in h.disks.iter().enumerate() {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO disk_sample
             (host_id, ts, mount, pos, total_bytes, used_bytes, avail_bytes, used_percent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            host_id,
            ts,
            d.mount,
            pos as i64,
            i(d.total_bytes),
            i(d.used_bytes),
            i(d.avail_bytes),
            d.used_percent,
        ])?;
    }
    for (pos, s) in sample.services.iter().enumerate() {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO service_sample
             (host_id, ts, service, pos, unit, up, cpu_percent, mem_current_bytes,
              mem_anon_bytes, mem_file_bytes, mem_peak_bytes, oom_kills, pids, restarts_seen,
              disk_bytes, disk_measured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?
        .execute(params![
            host_id,
            ts,
            s.name,
            pos as i64,
            s.unit,
            (s.state == ServiceState::Running) as i64,
            s.cpu_percent,
            i(s.mem_current_bytes),
            i(s.mem_anon_bytes),
            i(s.mem_file_bytes),
            i(s.mem_peak_bytes),
            i(s.oom_kills),
            i(s.pids),
            s.restarts_seen as i64,
            i(s.disk_bytes),
            i(s.disk_measured_at),
        ])?;
    }
    Ok(true)
}

fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Firing => "firing",
        EventKind::Repeat => "repeat",
        EventKind::Resolved => "resolved",
    }
}

fn kind_from(name: &str) -> EventKind {
    match name {
        "firing" => EventKind::Firing,
        "repeat" => EventKind::Repeat,
        _ => EventKind::Resolved,
    }
}

/// The newest sample for a host, rebuilt in the agent's shape, with the time
/// the server received it.
pub fn latest_sample(conn: &Connection, host: &HostRow) -> rusqlite::Result<Option<(Sample, u64)>> {
    let head = conn
        .prepare_cached(
            "SELECT ts, received_at, cpu_percent, cores, load1, load5, load15,
                    mem_total_bytes, mem_available_bytes, mem_used_bytes, mem_cache_bytes,
                    swap_total_bytes, swap_used_bytes, uptime_secs
             FROM host_sample WHERE host_id = ?1 ORDER BY ts DESC LIMIT 1",
        )?
        .query_row([host.id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                HostSample {
                    name: host.name.clone(),
                    cpu_percent: r.get(2)?,
                    cores: r.get::<_, Option<i64>>(3)?.map(|c| c.max(0) as u32),
                    load1: r.get(4)?,
                    load5: r.get(5)?,
                    load15: r.get(6)?,
                    mem_total_bytes: u(r.get(7)?),
                    mem_available_bytes: u(r.get(8)?),
                    mem_used_bytes: u(r.get(9)?),
                    mem_cache_bytes: u(r.get(10)?),
                    swap_total_bytes: u(r.get(11)?),
                    swap_used_bytes: u(r.get(12)?),
                    uptime_secs: u(r.get(13)?),
                    disks: Vec::new(),
                },
            ))
        })
        .optional()?;
    let Some((ts, received_at, mut host_sample)) = head else {
        return Ok(None);
    };

    host_sample.disks = conn
        .prepare_cached(
            "SELECT mount, total_bytes, used_bytes, avail_bytes, used_percent
             FROM disk_sample WHERE host_id = ?1 AND ts = ?2 ORDER BY pos",
        )?
        .query_map(params![host.id, ts], |r| {
            Ok(DiskSample {
                mount: r.get(0)?,
                total_bytes: u(r.get(1)?),
                used_bytes: u(r.get(2)?),
                avail_bytes: u(r.get(3)?),
                used_percent: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let services = conn
        .prepare_cached(
            "SELECT service, unit, up, cpu_percent, mem_current_bytes, mem_anon_bytes,
                    mem_file_bytes, mem_peak_bytes, oom_kills, pids, restarts_seen, disk_bytes,
                    disk_measured_at
             FROM service_sample WHERE host_id = ?1 AND ts = ?2 ORDER BY pos",
        )?
        .query_map(params![host.id, ts], |r| {
            Ok(ServiceSample {
                name: r.get(0)?,
                unit: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                state: if r.get::<_, i64>(2)? != 0 {
                    ServiceState::Running
                } else {
                    ServiceState::Down
                },
                cpu_percent: r.get(3)?,
                mem_current_bytes: u(r.get(4)?),
                mem_anon_bytes: u(r.get(5)?),
                mem_file_bytes: u(r.get(6)?),
                mem_peak_bytes: u(r.get(7)?),
                oom_kills: u(r.get(8)?),
                pids: u(r.get(9)?),
                restarts_seen: r.get::<_, Option<i64>>(10)?.unwrap_or(0).max(0) as u64,
                disk_bytes: u(r.get(11)?),
                disk_measured_at: u(r.get(12)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(Some((
        Sample {
            timestamp: ts.max(0) as u64,
            host: host_sample,
            services,
        },
        received_at.max(0) as u64,
    )))
}

/// Host points plus `(mount, points)` for each mount, as the agent's store
/// returns them.
pub type HostWindow = (Vec<HostPoint>, Vec<(String, Vec<DiskPoint>)>);

/// Host points and per-mount disk points with `from <= ts <= to`, oldest first.
pub fn host_points(
    conn: &Connection,
    host_id: i64,
    from: u64,
    to: u64,
) -> rusqlite::Result<HostWindow> {
    let window = params![host_id, clamp(from), clamp(to)];
    let points = conn
        .prepare_cached(
            "SELECT ts, cpu_percent, load1, load5, load15, mem_available_bytes, mem_used_bytes,
                    mem_cache_bytes, swap_used_bytes
             FROM host_sample WHERE host_id = ?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts",
        )?
        .query_map(window, |r| {
            Ok(HostPoint {
                t: r.get::<_, i64>(0)?.max(0) as u64,
                cpu_percent: pack_f32(r.get(1)?),
                load1: pack_f32(r.get(2)?),
                load5: pack_f32(r.get(3)?),
                load15: pack_f32(r.get(4)?),
                mem_available_bytes: pack_u64(u(r.get(5)?)),
                mem_used_bytes: pack_u64(u(r.get(6)?)),
                mem_cache_bytes: pack_u64(u(r.get(7)?)),
                swap_used_bytes: pack_u64(u(r.get(8)?)),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Mounts come out in the order the agent lists them, as its own API does.
    let mut disks: Vec<(String, Vec<DiskPoint>)> = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT mount, ts, used_bytes, used_percent
         FROM disk_sample WHERE host_id = ?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts, pos",
    )?;
    for row in stmt.query_map(window, |r| {
        Ok((
            r.get::<_, String>(0)?,
            DiskPoint {
                t: r.get::<_, i64>(1)?.max(0) as u64,
                used_bytes: pack_u64(u(r.get(2)?)),
                used_percent: pack_f32(r.get(3)?),
            },
        ))
    })? {
        let (mount, point) = row?;
        match disks.iter_mut().find(|(m, _)| *m == mount) {
            Some((_, points)) => points.push(point),
            None => disks.push((mount, vec![point])),
        }
    }
    Ok((points, disks))
}

/// Whether any sample for this service has ever been stored for the host.
pub fn service_exists(conn: &Connection, host_id: i64, service: &str) -> rusqlite::Result<bool> {
    conn.prepare_cached("SELECT 1 FROM service_sample WHERE host_id = ?1 AND service = ?2 LIMIT 1")?
        .query_row(params![host_id, service], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
}

pub fn service_points(
    conn: &Connection,
    host_id: i64,
    service: &str,
    from: u64,
    to: u64,
) -> rusqlite::Result<Vec<ServicePoint>> {
    conn.prepare_cached(
        "SELECT ts, up, cpu_percent, pids, mem_current_bytes, mem_anon_bytes, mem_file_bytes,
                oom_kills, disk_bytes
         FROM service_sample
         WHERE host_id = ?1 AND service = ?2 AND ts BETWEEN ?3 AND ?4 ORDER BY ts",
    )?
    .query_map(params![host_id, service, clamp(from), clamp(to)], |r| {
        Ok(ServicePoint {
            t: r.get::<_, i64>(0)?.max(0) as u64,
            up: r.get::<_, i64>(1)? != 0,
            cpu_percent: pack_f32(r.get(2)?),
            pids: pack_u32(u(r.get(3)?)),
            mem_current_bytes: pack_u64(u(r.get(4)?)),
            mem_anon_bytes: pack_u64(u(r.get(5)?)),
            mem_file_bytes: pack_u64(u(r.get(6)?)),
            oom_kills: pack_u64(u(r.get(7)?)),
            disk_bytes: pack_u64(u(r.get(8)?)),
        })
    })?
    .collect()
}

/// Newest first.
pub fn events(conn: &Connection, host_id: i64, limit: usize) -> rusqlite::Result<Vec<AlertEvent>> {
    conn.prepare_cached(
        "SELECT ts, rule, scope, metric, kind, value, op, threshold, held_secs
         FROM alert_event WHERE host_id = ?1 ORDER BY ts DESC, rule LIMIT ?2",
    )?
    .query_map(params![host_id, limit as i64], |r| {
        Ok(AlertEvent {
            timestamp: r.get::<_, i64>(0)?.max(0) as u64,
            rule: r.get(1)?,
            scope: r.get(2)?,
            metric: r.get(3)?,
            kind: kind_from(&r.get::<_, String>(4)?),
            value: r.get(5)?,
            op: r.get(6)?,
            threshold: r.get(7)?,
            held_secs: r.get::<_, Option<i64>>(8)?.unwrap_or(0).max(0) as u64,
        })
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use ninja_monitor::store::{unpack_f32, unpack_u64};

    pub fn sample(t: u64, cpu: Option<f64>, anon: Option<u64>) -> Sample {
        Sample {
            timestamp: t,
            host: HostSample {
                name: "vps".into(),
                cpu_percent: cpu,
                cores: Some(2),
                load1: Some(0.33),
                load5: None,
                load15: Some(0.1),
                mem_total_bytes: Some(4_000),
                mem_available_bytes: Some(3_000),
                mem_used_bytes: Some(1_000),
                mem_cache_bytes: None,
                swap_total_bytes: Some(0),
                swap_used_bytes: Some(0),
                uptime_secs: Some(t / 1000),
                disks: vec![DiskSample {
                    mount: "/".into(),
                    total_bytes: Some(100),
                    used_bytes: Some(t),
                    avail_bytes: Some(50),
                    used_percent: Some(50.0),
                }],
            },
            services: vec![ServiceSample {
                name: "riot-proxy".into(),
                unit: "riot-proxy.service".into(),
                state: if anon.is_some() {
                    ServiceState::Running
                } else {
                    ServiceState::Down
                },
                cpu_percent: cpu,
                mem_current_bytes: anon.map(|a| a * 3),
                mem_anon_bytes: anon,
                mem_file_bytes: anon.map(|a| a * 2),
                mem_peak_bytes: None,
                oom_kills: Some(1),
                pids: Some(14),
                restarts_seen: 2,
                disk_bytes: Some(73_728),
                disk_measured_at: Some(t),
            }],
        }
    }

    fn event(t: u64, rule: &str) -> AlertEvent {
        AlertEvent {
            timestamp: t,
            rule: rule.into(),
            scope: "service:riot-proxy".into(),
            metric: "down".into(),
            kind: EventKind::Firing,
            value: Some(1.0),
            op: None,
            threshold: None,
            held_secs: 60,
        }
    }

    #[test]
    fn inserts_dedupes_and_reads_back() {
        let db = Db::in_memory().unwrap();
        let samples = [
            sample(1000, Some(10.0), Some(100)),
            sample(2000, None, None),
            sample(3000, Some(30.0), Some(300)),
        ];
        let evs = [event(1000, "a"), event(3000, "b")];
        let first = db
            .call_sync(|c| {
                insert_batch(c, "vps", Some("ninja-monitor/0.1.0"), 5000, &samples, &evs)
            })
            .unwrap();
        assert_eq!(
            first,
            Inserted {
                accepted: 3,
                duplicates: 0,
                events: 2,
                previous_agent: None
            }
        );

        // The same batch again, plus one new sample: only the new one counts.
        let again = [
            samples[1].clone(),
            samples[2].clone(),
            sample(4000, Some(40.0), Some(400)),
        ];
        let second = db
            .call_sync(|c| insert_batch(c, "vps", Some("ninja-monitor/0.2.0"), 6000, &again, &evs))
            .unwrap();
        assert_eq!(second.accepted, 1);
        assert_eq!(second.duplicates, 2);
        assert_eq!(second.events, 0);
        assert_eq!(
            second.previous_agent,
            Some(Some("ninja-monitor/0.1.0".into()))
        );

        let host = db.call_sync(|c| host_by_name(c, "vps")).unwrap().unwrap();
        assert_eq!(host.first_seen, 5000);
        assert_eq!(host.last_seen, 6000);
        assert_eq!(host.agent.as_deref(), Some("ninja-monitor/0.2.0"));
        assert!(db.call_sync(|c| host_by_name(c, "nope")).unwrap().is_none());
        assert_eq!(db.call_sync(|c| hosts(c)).unwrap().len(), 1);

        let (latest, received_at) = db.call_sync(|c| latest_sample(c, &host)).unwrap().unwrap();
        assert_eq!(received_at, 6000);
        assert_eq!(
            serde_json::to_value(&latest).unwrap(),
            serde_json::to_value(&again[2]).unwrap()
        );

        let (points, disks) = db
            .call_sync(|c| host_points(c, host.id, 2000, 3000))
            .unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].t, 2000);
        assert_eq!(unpack_f32(points[0].cpu_percent), None);
        assert_eq!(unpack_f32(points[1].cpu_percent), Some(30.0));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].0, "/");
        assert_eq!(unpack_u64(disks[0].1[1].used_bytes), Some(3000));

        assert!(
            db.call_sync(|c| service_exists(c, host.id, "riot-proxy"))
                .unwrap()
        );
        assert!(
            !db.call_sync(|c| service_exists(c, host.id, "nope"))
                .unwrap()
        );
        let svc = db
            .call_sync(|c| service_points(c, host.id, "riot-proxy", 0, u64::MAX))
            .unwrap();
        assert_eq!(svc.len(), 4);
        assert!(!svc[1].up);
        assert_eq!(unpack_u64(svc[1].mem_anon_bytes), None);
        assert_eq!(unpack_u64(svc[3].mem_anon_bytes), Some(400));

        let ev = db.call_sync(|c| events(c, host.id, 10)).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], evs[1]);
        assert_eq!(ev[1], evs[0]);
        assert_eq!(db.call_sync(|c| events(c, host.id, 1)).unwrap().len(), 1);
    }

    #[test]
    fn missing_agent_string_keeps_the_old_one() {
        let db = Db::in_memory().unwrap();
        db.call_sync(|c| insert_batch(c, "vps", Some("x/1"), 1, &[sample(1, None, None)], &[]))
            .unwrap();
        db.call_sync(|c| insert_batch(c, "vps", None, 2, &[sample(2, None, None)], &[]))
            .unwrap();
        let host = db.call_sync(|c| host_by_name(c, "vps")).unwrap().unwrap();
        assert_eq!(host.agent.as_deref(), Some("x/1"));
        assert_eq!(host.last_seen, 2);
    }

    #[test]
    fn silent_since_round_trips() {
        let db = Db::in_memory().unwrap();
        db.call_sync(|c| insert_batch(c, "vps", None, 1, &[sample(1, None, None)], &[]))
            .unwrap();
        let host = db.call_sync(|c| host_by_name(c, "vps")).unwrap().unwrap();
        assert_eq!(host.silent_since, None);
        db.call_sync(|c| set_silent_since(c, host.id, Some(42)))
            .unwrap();
        assert_eq!(
            db.call_sync(|c| host_by_name(c, "vps"))
                .unwrap()
                .unwrap()
                .silent_since,
            Some(42)
        );
        db.call_sync(|c| set_silent_since(c, host.id, None))
            .unwrap();
        assert_eq!(
            db.call_sync(|c| host_by_name(c, "vps"))
                .unwrap()
                .unwrap()
                .silent_since,
            None
        );
    }

    #[test]
    fn sweep_removes_only_old_rows_in_batches() {
        let db = Db::in_memory().unwrap();
        let samples: Vec<Sample> = (1..=10)
            .map(|t| sample(t * 1000, Some(1.0), Some(1)))
            .collect();
        let evs: Vec<AlertEvent> = (1..=10).map(|t| event(t * 1000, "r")).collect();
        db.call_sync(|c| insert_batch(c, "vps", None, 20_000, &samples, &evs))
            .unwrap();
        let host = db.call_sync(|c| host_by_name(c, "vps")).unwrap().unwrap();

        // Samples older than 5s, events older than 8s, at 3 rows per statement.
        let pauses = std::cell::Cell::new(0);
        let swept = db
            .call_sync(|c| sweep(c, 10_000, 5_000, 8_000, 3, || pauses.set(pauses.get() + 1)))
            .unwrap();
        // ts 1000..=4000 go: 4 rows in each of three sample tables.
        assert_eq!(
            swept,
            Swept {
                samples: 12,
                events: 1
            }
        );
        assert!(pauses.get() >= 3, "{}", pauses.get());

        let (points, disks) = db
            .call_sync(|c| host_points(c, host.id, 0, u64::MAX))
            .unwrap();
        assert_eq!(
            points.iter().map(|p| p.t).collect::<Vec<_>>(),
            [5000, 6000, 7000, 8000, 9000, 10_000]
        );
        assert_eq!(disks[0].1.len(), 6);
        assert_eq!(
            db.call_sync(|c| service_points(c, host.id, "riot-proxy", 0, u64::MAX))
                .unwrap()
                .len(),
            6
        );
        assert_eq!(db.call_sync(|c| events(c, host.id, 100)).unwrap().len(), 9);
        assert!(
            db.call_sync(|c| host_by_name(c, "vps")).unwrap().is_some(),
            "host rows stay"
        );

        // Nothing old left: a second sweep is a no-op.
        let again = db
            .call_sync(|c| sweep(c, 10_000, 5_000, 8_000, 3, || {}))
            .unwrap();
        assert_eq!(again, Swept::default());
        db.call_sync(|c| checkpoint(c)).unwrap();
    }
}
