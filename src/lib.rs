//! ninja-monitor: a lightweight per-service resource monitor.

pub mod api;
pub mod config;
pub mod sample;
pub mod sampler;
pub mod store;

use std::future::Future;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::sampler::Sampler;
use crate::store::Store;

/// Samples waiting for the slowest subscriber before it starts losing them.
const BUS_CAPACITY: usize = 16;

/// Wires the sampler, bus and subscribers together and serves the API until
/// `shutdown` resolves.
pub async fn run(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (bus, _) = broadcast::channel(BUS_CAPACITY);
    let store = Arc::new(Store::new());

    // Subscribe before the sampler starts so the first sample is not missed.
    let tasks = [
        tokio::spawn(store::run(store.clone(), bus.subscribe())),
        tokio::spawn(sampler::run(
            Sampler::new(&config),
            config.sampling.interval,
            bus,
        )),
    ];

    let app = api::router(api::AppState::new(store));
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed");

    for task in tasks {
        task.abort();
    }
    result
}
