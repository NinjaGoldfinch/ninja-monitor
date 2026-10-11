//! ninja-ingest: central store and API for samples pushed by ninja-monitor
//! agents.

pub mod api;
pub mod check;
pub mod config;
pub mod db;
pub mod token;

use std::future::Future;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing::warn;

use crate::config::{Config, Secrets};
use crate::db::Db;

/// Serves the API on `listener` until `shutdown` resolves.
pub async fn run(
    config: Config,
    secrets: Secrets,
    db: Db,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    if config.agents.is_empty() {
        warn!("no [[agent]] configured; nothing can push samples until one is added");
    }
    for agent in &config.agents {
        if secrets.token_for(&agent.name).is_none() {
            warn!(
                agent = %agent.name,
                "{} is not set; this agent cannot authenticate until it is",
                agent.token_env
            );
        }
    }

    let state = api::AppState::new(db);
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")
}
