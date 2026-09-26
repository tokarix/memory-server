//! Trusted configuration, repository identity, and private state for future hooks.

#[cfg(target_os = "linux")]
pub mod config;
pub mod error;
#[cfg(target_os = "linux")]
pub mod identity;
pub mod limits;
#[cfg(target_os = "linux")]
pub mod state;

#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(not(target_os = "linux"))]
pub mod config {
    //! Unsupported-platform configuration API.
    pub use crate::unsupported::TrustedHooksConfig;
}
#[cfg(not(target_os = "linux"))]
pub mod identity {
    //! Unsupported-platform repository API.
    pub use crate::unsupported::{RepositoryIdentity, ResolvedBinding};
}
#[cfg(not(target_os = "linux"))]
pub mod state {
    //! Unsupported-platform state API.
    pub use crate::unsupported::{PrivateStateStore, StateKey};
}

pub use config::TrustedHooksConfig;
pub use error::{Error, Result};
pub use identity::{RepositoryIdentity, ResolvedBinding};
pub use state::{PrivateStateStore, StateKey};
