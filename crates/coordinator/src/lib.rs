//! The coordinator: owns state, serves the API, and drives workers.

use std::sync::Arc;

use anyhow::{Context, Result};
use repoman_common::config::CoordinatorConfig;
use repoman_store::Store;
use tokio::net::TcpListener;

pub mod api;
pub mod hub;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<CoordinatorConfig>,
    pub store: Arc<Store>,
    pub hub: Arc<hub::Hub>,
}

/// Open the store under `config.data_dir` and serve on `config.listen`.
pub async fn run(config: CoordinatorConfig) -> Result<()> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let store = Arc::new(Store::open(&config.data_dir.join("repoman.db"))?);
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, "coordinator listening");
    serve(config, store, listener).await
}

/// Serve on an already-bound listener. Tests use this with port 0.
pub async fn serve(config: CoordinatorConfig, store: Arc<Store>, listener: TcpListener) -> Result<()> {
    let config = Arc::new(config);
    let hub = hub::Hub::new(store.clone(), &config);
    tokio::spawn(hub.clone().dispatcher());
    let state = AppState { config, store, hub };
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
