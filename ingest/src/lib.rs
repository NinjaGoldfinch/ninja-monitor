//! ninja-ingest: central store and API for samples pushed by ninja-monitor
//! agents.

pub mod api;
pub mod check;
pub mod config;
pub mod db;
pub mod heartbeat;
pub mod ingest;
pub mod silence;
pub mod store;
pub mod sweep;
pub mod token;

use std::future::Future;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing::{info, warn};

use ninja_monitor::alerts::notify::{self, Discord, Queue};

use crate::config::{Config, DISCORD_WEBHOOK_ENV, Secrets};
use crate::db::Db;

/// Serves the API on `listener` until `shutdown` resolves, with the silence
/// watcher, retention sweeper and heartbeat running alongside.
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
    let known = db.call(|c| store::hosts(c)).await?;
    for agent in &config.agents {
        if !known.iter().any(|h| h.name == agent.name) {
            warn!(agent = %agent.name, "configured but never heard from yet");
        }
    }

    let http = notify::http_agent();
    let mut tasks = Vec::new();

    let queue = match (&secrets.discord_webhook, config.discord.enabled) {
        (Some(url), true) => {
            let queue = Arc::new(Queue::new(notify::QUEUE_CAPACITY));
            if config.discord.startup_message {
                queue.push(format!(
                    "**STARTED** [ninja-ingest] ninja-ingest {} is running, watching {} host{}",
                    env!("CARGO_PKG_VERSION"),
                    config.agents.len(),
                    if config.agents.len() == 1 { "" } else { "s" }
                ));
            }
            let discord = Discord::new(http.clone(), url.clone());
            tasks.push(tokio::spawn(notify::run(queue.clone(), discord)));
            Some(queue)
        }
        (None, true) => {
            warn!(
                "discord.enabled is true but {DISCORD_WEBHOOK_ENV} is not set; running without notifications"
            );
            None
        }
        (_, false) => None,
    };

    tasks.push(tokio::spawn(silence::run(
        db.clone(),
        config.silence.clone(),
        queue,
    )));
    tasks.push(tokio::spawn(sweep::run(db.clone(), config.storage.clone())));

    if config.heartbeat.enabled {
        match &secrets.heartbeat_url {
            Some(url) => {
                info!(interval = ?config.heartbeat.interval, "heartbeat enabled");
                tasks.push(tokio::spawn(heartbeat::run(
                    db.clone(),
                    http,
                    url.clone(),
                    config.heartbeat.interval,
                )));
            }
            None => warn!(
                "heartbeat.enabled is true but {} is not set; running without a heartbeat",
                config.heartbeat.url_env
            ),
        }
    }

    let state = api::AppState::new(db, Arc::new(config), Arc::new(secrets));
    let result = axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed");
    for task in tasks {
        task.abort();
    }
    result
}
