//! Trusted configuration, repository identity, and private state for future hooks.

#[cfg(target_os = "linux")]
pub mod config;
pub mod error;
#[cfg(target_os = "linux")]
pub mod identity;
pub mod limits;

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

pub use config::TrustedHooksConfig;
pub use error::{Error, Result};
pub use identity::{RepositoryIdentity, ResolvedBinding};
