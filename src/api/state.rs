//! Shared application state for the API server.

use std::sync::Arc;
use sui_store::LocalStore;

/// Application state shared across all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Option<Arc<LocalStore>>,
}

impl AppState {
    /// Create state with a connected local store.
    pub fn with_store(store: LocalStore) -> Self {
        Self {
            store: Some(Arc::new(store)),
        }
    }

    /// Create state without a store (stub mode).
    pub fn stub() -> Self {
        Self { store: None }
    }
}
