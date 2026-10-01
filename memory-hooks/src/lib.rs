//! Trusted configuration, repository identity, and private state for future hooks.

#[cfg(target_os = "linux")]
pub(crate) mod audit;
#[cfg(target_os = "linux")]
pub mod config;
#[cfg(target_os = "linux")]
pub mod delegated;
pub mod error;
#[cfg(target_os = "linux")]
pub mod execution_input;
#[cfg(target_os = "linux")]
pub mod identity;
#[cfg(target_os = "linux")]
pub mod installation;
pub mod limits;
#[cfg(target_os = "linux")]
pub mod pre_tool;
#[cfg(target_os = "linux")]
pub mod rust_build;
#[cfg(target_os = "linux")]
pub mod session_identity;
#[cfg(target_os = "linux")]
pub mod session_start;
#[cfg(target_os = "linux")]
pub mod snapshot;
#[cfg(target_os = "linux")]
pub mod state;
#[cfg(target_os = "linux")]
pub mod storage;
#[cfg(target_os = "linux")]
pub mod supervisor;

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
#[cfg(target_os = "linux")]
pub use installation::{ActiveInstallation, Installation, Lifecycle};
#[cfg(target_os = "linux")]
pub use snapshot::{DeliveryState, PendingGeneration, ValidatedSnapshot};
pub use state::{PrivateStateStore, StateKey};
