//! Explicit failure on platforms without the Linux trust and durability contract.

use std::path::{Path, PathBuf};

use memory_common::policy::ResolutionContext;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// A configuration cannot be loaded on this platform.
pub struct TrustedHooksConfig;

impl TrustedHooksConfig {
    /// Reject an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn load(_path: &Path) -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }
    /// Reject resolution on an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn resolve(&self, _cwd: &Path) -> Result<ResolvedBinding> {
        Err(Error::UnsupportedPlatform)
    }
    /// Versioned fingerprint; no instance can be loaded on this platform.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        ""
    }
}

/// A local identity cannot be established on this platform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryIdentity {
    /// Git identity, unavailable here.
    Git(PathBuf),
    /// Directory identity, unavailable here.
    Directory(PathBuf),
}

/// A binding cannot be resolved on this platform.
#[derive(Clone)]
pub struct ResolvedBinding {
    identity: RepositoryIdentity,
    worktree_root: PathBuf,
    effective_cwd: PathBuf,
    project: String,
    context: ResolutionContext,
    fingerprint: String,
}

impl ResolvedBinding {
    /// Identity of a binding, if one could exist.
    #[must_use]
    pub const fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }
    /// Canonical worktree root, if one could exist.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }
    /// Effective cwd, if one could exist.
    #[must_use]
    pub fn effective_cwd(&self) -> &Path {
        &self.effective_cwd
    }
    /// Explicit project, if one could exist.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }
    /// Trusted context, if one could exist.
    #[must_use]
    pub const fn context(&self) -> &ResolutionContext {
        &self.context
    }
    /// Binding fingerprint, if one could exist.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    /// Reject representation on an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn public_json(&self) -> Result<String> {
        Err(Error::UnsupportedPlatform)
    }
}

/// A state key cannot be constructed on this platform.
pub struct StateKey;

impl StateKey {
    /// Reject a state key on an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn new(_binding: &ResolvedBinding, _client: &str, _session: &str) -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }
    /// Digest, if a key could exist.
    #[must_use]
    pub fn digest(&self) -> &str {
        ""
    }
}

/// No filesystem store is available on this platform.
pub struct PrivateStateStore;

impl PrivateStateStore {
    /// Reject store initialization on an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn open(_config: &TrustedHooksConfig) -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }
    /// Reject transactions on an unsupported platform.
    ///
    /// # Errors
    /// Always returns `unsupported_platform`.
    pub fn transact<T, R, F>(&self, _key: &StateKey, _change: F) -> Result<R>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(Option<T>) -> Result<(Option<T>, R)>,
    {
        Err(Error::UnsupportedPlatform)
    }
}
