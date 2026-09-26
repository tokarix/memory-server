//! Trusted configuration, repository identity, and private state for future hooks.

#[cfg(target_os = "linux")]
pub mod config;
pub mod error;
pub mod limits;

#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(not(target_os = "linux"))]
pub mod config {
    //! Unsupported-platform configuration API.
    pub use crate::unsupported::TrustedHooksConfig;
}

pub use config::TrustedHooksConfig;
pub use error::{Error, Result};
