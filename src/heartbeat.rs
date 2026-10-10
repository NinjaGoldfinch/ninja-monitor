//! Outbound "still alive" ping, independent of sampling and alerts.

use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::alerts::notify::describe_error;

pub async fn run(agent: ureq::Agent, url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let (agent, url) = (agent.clone(), url.clone());
        let result = tokio::task::spawn_blocking(move || match agent.get(&url).call() {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(format!("HTTP {}", response.status().as_u16())),
            Err(err) => Err(describe_error(&err)),
        })
        .await;
        match result {
            Ok(Ok(())) => debug!("heartbeat sent"),
            Ok(Err(reason)) => warn!(%reason, "heartbeat failed"),
            Err(err) => warn!(%err, "heartbeat task failed"),
        }
    }
}
