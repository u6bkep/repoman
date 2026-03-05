pub mod repo_state;
pub mod srcinfo_cache;

// Re-export main types
pub use repo_state::RepoState;
pub use srcinfo_cache::SrcInfoCache;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::api::ServerEvent;
use crate::aur::AurClient;
use crate::build::BuildSystem;
use crate::config::Config;
use crate::db::Database;
use crate::models::BuildStage;
use crate::repo::RepoManager;

/// V2 AppState: per-field locks instead of one RwLock wrapping everything.
pub struct AppState {
    // Persistent (no lock needed -- these are internally concurrent or immutable)
    pub db: Database,
    pub config: Config,
    pub aur: AurClient,
    pub build: BuildSystem,
    pub repo: RepoManager,

    // Cached filesystem state
    pub srcinfo_cache: RwLock<SrcInfoCache>,
    pub repo_state: RwLock<RepoState>,

    // Transient build tracking
    pub active_builds: RwLock<HashSet<String>>,
    pub build_stages: RwLock<HashMap<String, BuildStage>>,
    pub build_cancel_flags: RwLock<HashMap<String, Arc<AtomicBool>>>,

    // Events + serialization
    pub events: broadcast::Sender<ServerEvent>,
    pub repo_mutex: Mutex<()>,
}

pub type SharedState = Arc<AppState>;
