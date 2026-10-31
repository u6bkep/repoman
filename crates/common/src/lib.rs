//! Types shared by every repoman role: configuration, identifiers, clock
//! formatting, and the coordinator/worker wire protocol.

pub mod clock;
pub mod config;
pub mod ids;
pub mod proto;

pub use ids::new_id;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
