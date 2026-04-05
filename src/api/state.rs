//! Shared application state for the API server.

use std::sync::Arc;

/// Application state shared across all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Option<Arc<dyn sui_store::Store>>,
}

impl AppState {
    /// Create state with a connected store (any backend).
    pub fn with_store(store: impl sui_store::Store + 'static) -> Self {
        Self {
            store: Some(Arc::new(store)),
        }
    }

    /// Create state without a store (stub mode).
    pub fn stub() -> Self {
        Self { store: None }
    }
}
