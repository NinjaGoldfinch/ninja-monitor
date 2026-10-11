//! HTTP API: the authenticated ingest route and the unauthenticated read
//! routes, which mirror the agent's API with a host dimension added.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
    AUTHORIZATION, HeaderValue, ORIGIN, VARY,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use ninja_monitor::alerts::AlertEvent;
use ninja_monitor::config::Scope;
use ninja_monitor::history;
use ninja_monitor::sampler::unix_millis;

use crate::config::{Config, Secrets};
use crate::db::Db;
use crate::ingest::{self, IngestBody, Reject};
use crate::store::{self, HostRow};

const DEFAULT_MINUTES: u64 = 60;
const DEFAULT_MAX_POINTS: u64 = 720;
const MAX_MAX_POINTS: u64 = 2_000;
const DEFAULT_EVENT_LIMIT: u64 = 100;
const MAX_EVENT_LIMIT: u64 = 1_000;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    pub started: Instant,
    /// Agent version strings seen since start, to log first contact and changes.
    seen_agents: Arc<Mutex<HashMap<String, String>>>,
}

impl AppState {
    pub fn new(db: Db, config: Arc<Config>, secrets: Arc<Secrets>) -> Self {
        Self {
            db,
            config,
            secrets,
            started: Instant::now(),
            seen_agents: Arc::default(),
        }
    }

    fn retention_ms(&self) -> u64 {
        self.config.storage.retention.as_millis() as u64
    }
}

pub fn router(state: AppState) -> Router {
    let read = Router::new()
        .route("/healthz", get(healthz))
        .route("/hosts", get(hosts))
        .route("/hosts/{host}/stats", get(host_stats))
        .route("/hosts/{host}/history", get(host_history))
        .route("/hosts/{host}/alerts", get(host_alerts))
        .route("/stats", get(stats))
        .layer(middleware::from_fn_with_state(state.clone(), cors));
    Router::new()
        .route("/v1/ingest", post(ingest_route))
        .merge(read)
        .fallback(not_found)
        .with_state(state)
}

/// Adds CORS headers on the read routes for origins listed in the config.
/// Nothing is added for other origins, and the ingest route never gets any.
async fn cors(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let allowed = req
        .headers()
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| state.config.server.cors_origins.iter().any(|a| a == o))
        .and_then(|o| HeaderValue::from_str(o).ok());
    let preflight = req.method() == Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    if let Some(origin) = allowed {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(VARY, HeaderValue::from_static("Origin"));
        if preflight {
            headers.insert(
                ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, OPTIONS"),
            );
            headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        }
    }
    response
}

async fn healthz(State(state): State<AppState>) -> Response {
    let hosts = state.db.host_count().await;
    let db_bytes = state.db.size_bytes().await;
    match (hosts, db_bytes) {
        (Ok(hosts), Ok(db_bytes)) => Json(json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": state.started.elapsed().as_secs(),
            "hosts": hosts,
            "db_bytes": db_bytes,
        }))
        .into_response(),
        (Err(err), _) | (_, Err(err)) => db_error(err),
    }
}

async fn ingest_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IngestBody>, JsonRejection>,
) -> Response {
    let auth = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    let Some(host) = ingest::authenticate(auth, &state.secrets) else {
        return error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            // axum answers 422 for a body that parses but does not fit; the
            // protocol calls every malformed body a 400.
            let status = match rejection.status() {
                StatusCode::UNPROCESSABLE_ENTITY => StatusCode::BAD_REQUEST,
                other => other,
            };
            return error(status, &rejection.body_text());
        }
    };
    let now = unix_millis();
    let checked = match ingest::validate(body, &host, now, state.retention_ms()) {
        Ok(checked) => checked,
        Err(Reject::TooLarge(msg)) => return error(StatusCode::PAYLOAD_TOO_LARGE, &msg),
        Err(Reject::Bad(msg)) => return error(StatusCode::BAD_REQUEST, &msg),
    };

    let agent = checked.agent.clone();
    let host_name = host.clone();
    let inserted = state
        .db
        .call(move |c| {
            store::insert_batch(
                c,
                &host_name,
                agent.as_deref(),
                now,
                &checked.samples,
                &checked.events,
            )
        })
        .await;
    let inserted = match inserted {
        Ok(inserted) => inserted,
        Err(err) => return db_error(err),
    };

    if let Some(agent) = &checked.agent {
        let mut seen = state.seen_agents.lock().unwrap_or_else(|e| e.into_inner());
        match seen.insert(host.clone(), agent.clone()) {
            None => info!(host, agent, "agent connected"),
            Some(previous) if previous != *agent => {
                warn!(host, from = previous, to = agent, "agent version changed")
            }
            Some(_) => {}
        }
    }

    Json(json!({
        "accepted": inserted.accepted,
        "duplicates": inserted.duplicates,
        "rejected": checked.rejected,
        "events_accepted": inserted.events,
    }))
    .into_response()
}

#[derive(Serialize)]
struct HostSummary {
    name: String,
    status: &'static str,
    first_seen: Option<u64>,
    last_seen: Option<u64>,
    silent_since: Option<u64>,
    agent: Option<String>,
}

impl HostSummary {
    fn known(row: &HostRow, now: u64, after_ms: u64) -> Self {
        let silent = now.saturating_sub(row.last_seen) >= after_ms;
        Self {
            name: row.name.clone(),
            status: if silent { "silent" } else { "reporting" },
            first_seen: Some(row.first_seen),
            last_seen: Some(row.last_seen),
            silent_since: silent.then(|| row.silent_since.unwrap_or(row.last_seen + after_ms)),
            agent: row.agent.clone(),
        }
    }

    fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            status: "unknown",
            first_seen: None,
            last_seen: None,
            silent_since: None,
            agent: None,
        }
    }
}

/// Every host that has reported plus every configured agent that has not.
async fn all_hosts(state: &AppState) -> anyhow::Result<(Vec<HostRow>, Vec<HostSummary>)> {
    let rows = state.db.call(|c| store::hosts(c)).await?;
    let now = unix_millis();
    let after = state.config.silence.after.as_millis() as u64;
    let mut summaries: Vec<HostSummary> = rows
        .iter()
        .map(|row| HostSummary::known(row, now, after))
        .collect();
    for agent in &state.config.agents {
        if !rows.iter().any(|r| r.name == agent.name) {
            summaries.push(HostSummary::unknown(&agent.name));
        }
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((rows, summaries))
}

async fn hosts(State(state): State<AppState>) -> Response {
    match all_hosts(&state).await {
        Ok((_, summaries)) => Json(json!({ "hosts": summaries })).into_response(),
        Err(err) => db_error(err),
    }
}

/// One call for a dashboard: every host's summary with its latest sample.
async fn stats(State(state): State<AppState>) -> Response {
    let (rows, summaries) = match all_hosts(&state).await {
        Ok(found) => found,
        Err(err) => return db_error(err),
    };
    let mut out = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let mut value = serde_json::to_value(&summary).unwrap_or_default();
        let latest = match rows.iter().find(|r| r.name == summary.name) {
            Some(row) => {
                let row = row.clone();
                match state.db.call(move |c| store::latest_sample(c, &row)).await {
                    Ok(latest) => latest,
                    Err(err) => return db_error(err),
                }
            }
            None => None,
        };
        match latest {
            Some((sample, received_at)) => {
                value["sample"] = serde_json::to_value(&sample).unwrap_or_default();
                value["received_at"] = json!(received_at);
            }
            None => {
                value["sample"] = Value::Null;
                value["received_at"] = Value::Null;
            }
        }
        out.push(value);
    }
    Json(json!({ "hosts": out })).into_response()
}

enum Resolved {
    /// Has reported at least once.
    Known(HostRow),
    /// Configured on this server but never heard from.
    Configured,
}

async fn resolve_host(state: &AppState, name: &str) -> Result<Resolved, Fail> {
    let owned = name.to_string();
    match state.db.call(move |c| store::host_by_name(c, &owned)).await {
        Ok(Some(row)) => Ok(Resolved::Known(row)),
        Ok(None) if state.config.agents.iter().any(|a| a.name == name) => Ok(Resolved::Configured),
        Ok(None) => Err(fail(
            StatusCode::NOT_FOUND,
            &format!("unknown host `{name}`"),
        )),
        Err(err) => Err(db_fail(err)),
    }
}

async fn host_stats(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let row = match resolve_host(&state, &name).await {
        Ok(Resolved::Known(row)) => row,
        Ok(Resolved::Configured) => return no_samples(),
        Err(f) => return failed(f),
    };
    match state.db.call(move |c| store::latest_sample(c, &row)).await {
        Ok(Some((sample, received_at))) => {
            let mut value = serde_json::to_value(&sample).unwrap_or_default();
            value["received_at"] = json!(received_at);
            Json(value).into_response()
        }
        Ok(None) => no_samples(),
        Err(err) => db_error(err),
    }
}

/// A failure to turn into an error response. Kept small so it can travel in
/// a `Result` without clippy objecting to the size of `Response`.
type Fail = (StatusCode, String);

fn fail(status: StatusCode, message: &str) -> Fail {
    (status, message.to_string())
}

fn db_fail(err: anyhow::Error) -> Fail {
    tracing::error!(err = format!("{err:#}"), "database error");
    fail(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
}

fn no_samples() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, "no samples received yet")
}

#[derive(Deserialize)]
struct HistoryParams {
    scope: Option<String>,
    minutes: Option<String>,
    max_points: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum HistoryData {
    Host(history::HostHistory),
    Service(history::ServiceHistory),
}

/// The same shape as the agent's `/history` response.
#[derive(Serialize)]
struct HistoryResponse {
    scope: String,
    from: u64,
    to: u64,
    minutes: u64,
    #[serde(flatten)]
    data: HistoryData,
}

struct Window {
    from: u64,
    to: u64,
    minutes: u64,
}

/// Either `minutes` (ending now) or an explicit `from` and `to`, never
/// longer than the retention window.
fn window(params: &HistoryParams, now: u64, retention_ms: u64) -> Result<Window, Fail> {
    let bad = |msg: &str| Err(fail(StatusCode::BAD_REQUEST, msg));
    match (&params.from, &params.to) {
        (None, None) => {
            let retention_minutes = retention_ms.div_ceil(60_000).max(1);
            let Some(minutes) = parse_positive(params.minutes.as_deref(), DEFAULT_MINUTES) else {
                return bad("`minutes` must be a positive integer");
            };
            let minutes = minutes.min(retention_minutes);
            Ok(Window {
                from: now.saturating_sub(minutes * 60_000),
                to: now,
                minutes,
            })
        }
        (Some(from), Some(to)) => {
            if params.minutes.is_some() {
                return bad("use either `minutes` or `from` and `to`, not both");
            }
            let (Ok(from), Ok(to)) = (from.parse::<u64>(), to.parse::<u64>()) else {
                return bad("`from` and `to` must be Unix milliseconds");
            };
            if from >= to {
                return bad("`from` must be before `to`");
            }
            if to - from > retention_ms {
                return bad("window is longer than the retention period");
            }
            Ok(Window {
                from,
                to,
                minutes: (to - from).div_ceil(60_000),
            })
        }
        _ => bad("`from` and `to` must be given together"),
    }
}

async fn host_history(
    State(state): State<AppState>,
    Path(name): Path<String>,
    params: Result<Query<HistoryParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return error(StatusCode::BAD_REQUEST, "malformed query string");
    };
    let Some(scope_text) = params.scope.clone() else {
        return error(StatusCode::BAD_REQUEST, "missing `scope`");
    };
    let Some(max_points) = parse_positive(params.max_points.as_deref(), DEFAULT_MAX_POINTS) else {
        return error(
            StatusCode::BAD_REQUEST,
            "`max_points` must be a positive integer",
        );
    };
    let max_points = max_points.min(MAX_MAX_POINTS) as usize;
    let window = match window(&params, unix_millis(), state.retention_ms()) {
        Ok(window) => window,
        Err(f) => return failed(f),
    };
    let unknown_scope = || {
        error(
            StatusCode::NOT_FOUND,
            &format!("unknown scope `{scope_text}`"),
        )
    };
    let Ok(scope) = Scope::try_from(scope_text.clone()) else {
        return unknown_scope();
    };

    let row = match resolve_host(&state, &name).await {
        Ok(Resolved::Known(row)) => Some(row),
        Ok(Resolved::Configured) => None,
        Err(f) => return failed(f),
    };
    let (from, to) = (window.from, window.to);
    let data = match (&scope, row) {
        (Scope::Host, None) => HistoryData::Host(history::host(&[], &[], max_points)),
        (Scope::Service(_), None) => return unknown_scope(),
        (Scope::Host, Some(row)) => {
            match state
                .db
                .call(move |c| store::host_points(c, row.id, from, to))
                .await
            {
                Ok((points, disks)) => {
                    HistoryData::Host(history::host(&points, &disks, max_points))
                }
                Err(err) => return db_error(err),
            }
        }
        (Scope::Service(service), Some(row)) => {
            let service = service.clone();
            let result = state
                .db
                .call(move |c| {
                    if !store::service_exists(c, row.id, &service)? {
                        return Ok(None);
                    }
                    store::service_points(c, row.id, &service, from, to).map(Some)
                })
                .await;
            match result {
                Ok(Some(points)) => HistoryData::Service(history::service(&points, max_points)),
                Ok(None) => return unknown_scope(),
                Err(err) => return db_error(err),
            }
        }
    };
    Json(HistoryResponse {
        scope: scope.to_string(),
        from,
        to,
        minutes: window.minutes,
        data,
    })
    .into_response()
}

#[derive(Deserialize)]
struct AlertParams {
    limit: Option<String>,
}

async fn host_alerts(
    State(state): State<AppState>,
    Path(name): Path<String>,
    params: Result<Query<AlertParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return error(StatusCode::BAD_REQUEST, "malformed query string");
    };
    let Some(limit) = parse_positive(params.limit.as_deref(), DEFAULT_EVENT_LIMIT) else {
        return error(
            StatusCode::BAD_REQUEST,
            "`limit` must be a positive integer",
        );
    };
    let limit = limit.min(MAX_EVENT_LIMIT) as usize;
    let events: Vec<AlertEvent> = match resolve_host(&state, &name).await {
        Ok(Resolved::Known(row)) => {
            match state
                .db
                .call(move |c| store::events(c, row.id, limit))
                .await
            {
                Ok(events) => events,
                Err(err) => return db_error(err),
            }
        }
        Ok(Resolved::Configured) => Vec::new(),
        Err(f) => return failed(f),
    };
    Json(json!({ "events": events })).into_response()
}

fn parse_positive(text: Option<&str>, default: u64) -> Option<u64> {
    match text {
        None => Some(default),
        Some(t) => t.parse().ok().filter(|&n| n > 0),
    }
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn db_error(err: anyhow::Error) -> Response {
    tracing::error!(err = format!("{err:#}"), "database error");
    error(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn failed((status, message): Fail) -> Response {
    error(status, &message)
}
