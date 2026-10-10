//! Holds what the API serves. For now only the latest sample.

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast::{self, error::RecvError};
use tracing::warn;

use crate::sample::Sample;

#[derive(Default)]
pub struct Store {
    latest: RwLock<Option<Arc<Sample>>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn latest(&self) -> Option<Arc<Sample>> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn push(&self, sample: Arc<Sample>) {
        *self.latest.write().unwrap_or_else(|e| e.into_inner()) = Some(sample);
    }
}

/// Bus subscriber that feeds the store.
pub async fn run(store: Arc<Store>, mut rx: broadcast::Receiver<Arc<Sample>>) {
    loop {
        match rx.recv().await {
            Ok(sample) => store.push(sample),
            Err(RecvError::Lagged(missed)) => warn!(missed, "store fell behind the sampler"),
            Err(RecvError::Closed) => break,
        }
    }
}
