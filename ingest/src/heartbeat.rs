//! Outbound "still alive" ping for an external checker. Unlike the agent's,
//! it first proves the database answers, so a wedged server stops pinging.

use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use ninja_monitor::alerts::notify::describe_error;

use crate::db::Db;

/// How long the database may take to answer before the ping is skipped.
const DB_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(db: Db, agent: ureq::Agent, url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match tokio::time::timeout(DB_TIMEOUT, db.host_count()).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                warn!(
                    err = format!("{err:#}"),
                    "skipping heartbeat: database query failed"
                );
                continue;
            }
            Err(_) => {
                warn!("skipping heartbeat: database did not answer in time");
                continue;
            }
        }
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
