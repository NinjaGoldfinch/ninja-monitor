//! ninja-monitor: a lightweight per-service resource monitor.

pub mod alerts;
pub mod api;
pub mod check;
pub mod config;
pub mod heartbeat;
pub mod history;
pub mod init;
pub mod push;
pub mod sample;
pub mod sampler;
pub mod store;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::alerts::Engine;
use crate::alerts::notify::{self, Discord, Queue};
use crate::config::{Config, DISCORD_WEBHOOK_ENV, Secrets};
use crate::sampler::{Sampler, walk};
use crate::store::Store;

/// Samples waiting for the slowest subscriber before it starts losing them.
const BUS_CAPACITY: usize = 16;
/// Alert events waiting for the pusher.
const EVENT_BUS_CAPACITY: usize = 100;

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
    let (events_tx, _) = broadcast::channel(EVENT_BUS_CAPACITY);
    tasks.push(tokio::spawn(store::run(store.clone(), bus.subscribe())));

    let (stop_push, pusher) = match (&secrets.push_token, config.push.enabled) {
        (Some(token), true) => {
            let url = config.push.url.clone().unwrap_or_default();
            info!(
                buffer = config.push.capacity(config.sampling.interval),
                "push enabled"
            );
            let pusher = push::Pusher {
                state: push::State::new(&config.host.name, &config.push, config.sampling.interval),
                agent: agent.clone(),
                url,
                token: token.clone(),
                queue: queue.clone(),
            };
            let (stop_tx, stop_rx) = watch::channel(false);
            let handle = tokio::spawn(push::run(
                pusher,
                bus.subscribe(),
                events_tx.subscribe(),
                stop_rx,
            ));
            (Some(stop_tx), Some(handle))
        }
        (None, true) => {
            warn!(
                "push.enabled is true but {} is not set; running without pushing",
                config.push.token_env
            );
            (None, None)
        }
        (_, false) => (None, None),
    };

    tasks.push(tokio::spawn(alerts::run(
        engine.clone(),
        bus.subscribe(),
        queue,
        events_tx,
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

    if config.heartbeat.enabled {
        match &secrets.heartbeat_url {
            Some(url) => {
                info!(interval = ?config.heartbeat.interval, "heartbeat enabled");
                tasks.push(tokio::spawn(heartbeat::run(
                    agent,
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
    // Give the pusher one bounded chance to send what it still holds.
    if let (Some(stop), Some(handle)) = (stop_push, pusher) {
        let _ = stop.send(true);
        let grace = push::SHUTDOWN_FLUSH + std::time::Duration::from_millis(500);
        if tokio::time::timeout(grace, handle).await.is_err() {
            warn!("pusher did not finish its final flush in time");
        }
    }
    result
}
