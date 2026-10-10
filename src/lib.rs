//! ninja-monitor: a lightweight per-service resource monitor.

pub mod alerts;
pub mod api;
pub mod config;
pub mod history;
pub mod sample;
pub mod sampler;
pub mod store;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::alerts::Engine;
use crate::alerts::notify::{self, Discord, Queue};
use crate::config::{Config, DISCORD_WEBHOOK_ENV, Secrets};
use crate::sampler::{Sampler, walk};
use crate::store::Store;

/// Samples waiting for the slowest subscriber before it starts losing them.
const BUS_CAPACITY: usize = 16;

/// Wires the sampler, bus and subscribers together and serves the API until
/// `shutdown` resolves.
pub async fn run(
    config: Config,
    secrets: Secrets,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (bus, _) = broadcast::channel(BUS_CAPACITY);
    let store = Arc::new(Store::new(&config));
    let engine = Arc::new(Mutex::new(Engine::new(&config)));
    let stop_walks = Arc::new(AtomicBool::new(false));
    let agent = notify::http_agent();
    let mut tasks = Vec::new();

    let queue = match (&secrets.discord_webhook, config.discord.enabled) {
        (Some(url), true) => {
            let queue = Arc::new(Queue::new(notify::QUEUE_CAPACITY));
            if config.discord.startup_message {
                queue.push(format!(
                    "**STARTED** [{}] ninja-monitor {} is running",
                    config.host.name,
                    env!("CARGO_PKG_VERSION")
                ));
            }
            let discord = Discord::new(agent.clone(), url.clone());
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

    // Subscribe before the sampler starts so the first sample is not missed.
    tasks.push(tokio::spawn(store::run(store.clone(), bus.subscribe())));
    tasks.push(tokio::spawn(alerts::run(
        engine.clone(),
        bus.subscribe(),
        queue,
        config.host.name.clone(),
    )));

    let mut sampler = Sampler::new(&config);
    let service_paths: Vec<_> = config.services.iter().map(|s| s.paths.clone()).collect();
    if service_paths.iter().any(|p| !p.is_empty()) {
        let (tx, rx) = watch::channel(vec![None; service_paths.len()]);
        sampler = sampler.with_disk_usage(rx);
        tasks.push(tokio::spawn(walk::run(
            service_paths,
            config.sampling.disk_interval,
            tx,
            stop_walks.clone(),
        )));
    }
    tasks.push(tokio::spawn(sampler::run(
        sampler,
        config.sampling.interval,
        bus,
    )));

    let state = api::AppState::new(store, engine, config.sampling.history);
    let result = axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed");

    // A walk in progress runs on the blocking pool; ask it to finish early.
    stop_walks.store(true, Ordering::Relaxed);
    for task in tasks {
        task.abort();
    }
    result
}
