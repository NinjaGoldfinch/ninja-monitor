//! Loopback-only JSON API.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    store: Arc<Store>,
    started: Instant,
}

impl AppState {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            started: Instant::now(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
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

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
