//! Loopback-only JSON API.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::alerts::Engine;
use crate::config::Scope;
use crate::history;
use crate::sampler::unix_millis;
use crate::store::{Range, Store};

const DEFAULT_MINUTES: u64 = 60;
const DEFAULT_MAX_POINTS: usize = 720;

#[derive(Clone)]
pub struct AppState {
    store: Arc<Store>,
    engine: Arc<Mutex<Engine>>,
    started: Instant,
    history_minutes: u64,
}

impl AppState {
    pub fn new(store: Arc<Store>, engine: Arc<Mutex<Engine>>, history: Duration) -> Self {
        Self {
            store,
            engine,
            started: Instant::now(),
            history_minutes: history.as_secs().div_ceil(60).max(1),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
        .route("/history", get(history))
        .route("/alerts", get(alerts))
        .fallback(not_found)
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started.elapsed().as_secs(),
    }))
    .into_response()
}

async fn stats(State(state): State<AppState>) -> Response {
    match state.store.latest() {
        Some(sample) => Json(sample.as_ref()).into_response(),
        None => error(StatusCode::SERVICE_UNAVAILABLE, "no sample taken yet"),
    }
}

async fn alerts(State(state): State<AppState>) -> Response {
    let view = state
        .engine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .view();
    Json(view).into_response()
}

#[derive(Deserialize)]
struct HistoryParams {
    scope: Option<String>,
    minutes: Option<String>,
    max_points: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum HistoryData {
    Host(history::HostHistory),
    Service(history::ServiceHistory),
}

#[derive(Serialize)]
struct HistoryResponse {
    scope: String,
    from: u64,
    to: u64,
    minutes: u64,
    #[serde(flatten)]
    data: HistoryData,
}

async fn history(
    State(state): State<AppState>,
    params: Result<Query<HistoryParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return error(StatusCode::BAD_REQUEST, "malformed query string");
    };
    let Some(scope_text) = params.scope else {
        return error(StatusCode::BAD_REQUEST, "missing `scope`");
    };
    let minutes = match parse_positive(params.minutes.as_deref(), DEFAULT_MINUTES) {
        Some(m) => m.min(state.history_minutes),
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "`minutes` must be a positive integer",
            );
        }
    };
    let max_points = match parse_positive(params.max_points.as_deref(), DEFAULT_MAX_POINTS as u64) {
        Some(p) => usize::try_from(p).unwrap_or(usize::MAX),
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "`max_points` must be a positive integer",
            );
        }
    };
    let unknown = || {
        error(
            StatusCode::NOT_FOUND,
            &format!("unknown scope `{scope_text}`"),
        )
    };
    let Ok(scope) = Scope::try_from(scope_text.clone()) else {
        return unknown();
    };

    let to = unix_millis();
    let from = to.saturating_sub(minutes * 60_000);
    let data = match state.store.range(&scope, from, to) {
        Some(Range::Host { points, disks }) => {
            HistoryData::Host(history::host(&points, &disks, max_points))
        }
        Some(Range::Service(points)) => HistoryData::Service(history::service(&points, max_points)),
        None => return unknown(),
    };
    Json(HistoryResponse {
        scope: scope.to_string(),
        from,
        to,
        minutes,
        data,
    })
    .into_response()
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

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
