//! Pure-Rust readers for the pacman ecosystem's text formats, plus version
//! comparison with libalpm semantics. No libalpm linkage: the coordinator
//! runs in a container and the parsers must be testable anywhere.

pub mod depspec;
pub mod pacman_conf;
pub mod pkginfo;
pub mod srcinfo;
pub mod syncdb;
pub mod version;

pub use depspec::{Cmp, DepSpec};
pub use version::{Version, vercmp};
