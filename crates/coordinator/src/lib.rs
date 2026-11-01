//! The coordinator: owns state, serves the API, and drives workers.

use std::sync::Arc;

use anyhow::{Context, Result};
use repoman_common::config::CoordinatorConfig;
use repoman_store::Store;
use tokio::net::TcpListener;

pub mod api;
pub mod aurreq;
pub mod aurweb;
pub mod aursec;
pub mod aur;
pub mod blobs;
pub mod cache;
pub mod engine;
pub mod forge;
pub mod gate;
pub mod reviewer;
pub mod hub;
pub mod keys;
pub mod planner;
pub mod publisher;
pub mod streams;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<CoordinatorConfig>,
    pub store: Arc<Store>,
    pub hub: Arc<hub::Hub>,
    pub streams: Arc<streams::Streams>,
    pub aur: Arc<aur::Aur>,
    pub blobs: Arc<blobs::Blobs>,
    pub publisher: Arc<publisher::Publisher>,
    pub keys: Arc<keys::Keys>,
    pub reviewer: Arc<reviewer::Reviewer>,
    pub forge: Arc<forge::ForgeClient>,
    pub cache: Arc<cache::Cache>,
    pub aursec: Option<Arc<aursec::AurSecurity>>,
    pub aurreq: Option<Arc<aurreq::AurRequests>>,
    pub aurweb: Arc<aurweb::AurWeb>,
    pub engine: Arc<engine::Engine>,
}

impl AppState {
    pub fn new(config: CoordinatorConfig, store: Arc<Store>) -> Result<Self> {
        let config = Arc::new(config);
        std::fs::create_dir_all(config.bulk_dir())?;
        let hub = hub::Hub::new(store.clone(), &config);
        let streams = Arc::new(streams::Streams::new(config.clone())?);
        streams.load_cached()?;
        let aur = Arc::new(aur::Aur::new(&config.aur_rpc_url, &config.aur_git_url, &config.bulk_dir())?);
        let blobs = Arc::new(blobs::Blobs::new(&config.bulk_dir())?);
        let publisher = Arc::new(publisher::Publisher::new(&config)?);
        let keys = Arc::new(keys::Keys::new(&config.data_dir)?);
        let reviewer = Arc::new(reviewer::Reviewer::new(config.reviewer.clone()));
        let forge = Arc::new(forge::ForgeClient::new(std::env::var(&config.github_token_env).ok().filter(|t| !t.is_empty())));
        let cache = Arc::new(cache::Cache::new(config.clone(), streams.clone(), store.clone())?);
        let aursec = config.aur_security_url.as_deref().filter(|u| !u.is_empty()).map(|u| Arc::new(aursec::AurSecurity::new(u)));
        let aurweb = Arc::new(aurweb::AurWeb::new(&config.aur_web_url));
        let aurreq = config.aur_requests_url.as_deref().filter(|u| !u.is_empty()).map(|u| Arc::new(aurreq::AurRequests::new(u)));
        if reviewer.enabled() && !reviewer.is_configured() {
            tracing::warn!(env = %config.reviewer.api_key_env, "reviewer enabled but no API key in the environment; candidates will record no verdict");
        }
        Ok(AppState {
            config,
            store,
            hub,
            streams,
            aur,
            blobs,
            publisher,
            keys,
            reviewer,
            forge,
            cache,
            aursec,
            aurreq,
            aurweb,
            engine: engine::Engine::new(),
        })
    }
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
    let state = AppState::new(config, store)?;
    tokio::spawn(state.hub.clone().dispatcher());
    // Cache eviction on a schedule, independent of runs.
    let cache = state.cache.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            if let Err(e) = cache.maintain() {
                tracing::warn!(error = %format!("{e:#}"), "cache maintenance failed");
            }
        }
    });
    // aur-requests between runs: a request against one of our packages is
    // worth knowing about before the next run, and the feed is short.
    if state.aurreq.is_some() {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(st.config.aur_requests_poll_secs.max(60)));
            loop {
                tick.tick().await;
                match engine::ingest_aur_requests(&st).await {
                    Ok(n) if n > 0 => tracing::info!(new_signals = n, "aur-requests"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "aur-requests feed"),
                }
            }
        });
    }
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
