//! HTTP API. Phase 1 serves only `/healthz`; the ingest and read routes
//! follow.

use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::db::Db;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub started: Instant,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            started: Instant::now(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(not_found)
        .with_state(state)
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
        (Err(err), _) | (_, Err(err)) => {
            tracing::error!(%err, "healthz database query failed");
            error(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
