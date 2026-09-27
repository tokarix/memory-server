//! Fixed installation authority and monotonic configuration activation.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{BindingIdentity, ClientAdapter, TrustedHooksConfig, valid_path, within};
use crate::error::{Error, Result};
use crate::state::{PrivateDirectory, RootMode};

const MANIFEST: &str = "installation-manifest";
const JOURNAL: &str = "activation-journal";

/// The lifecycle of one pinned installation authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// No configuration is authoritative while an activation is in progress.
    Changing,
    /// A validated v2 configuration and root are selected.
    Active,
    /// The anchor permanently rejects future deliveries.
    Retired,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    installation_id: Uuid,
    anchor_path: PathBuf,
    client: ClientAdapter,
    epoch: u64,
    nonce: Uuid,
    lifecycle: Lifecycle,
    config_path: Option<PathBuf>,
    fingerprint: Option<String>,
    snapshot_root: Option<PathBuf>,
    snapshot_root_device: Option<u64>,
    snapshot_root_inode: Option<u64>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransitionKind {
    Activation,
    Retirement,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    epoch: u64,
    nonce: Uuid,
    kind: TransitionKind,
    resolved: bool,
}

/// Trusted active configuration paired with its current installation epoch.
pub struct ActiveInstallation {
    /// Protected configuration reloaded under the fixed installation lock.
    pub config: TrustedHooksConfig,
    /// Monotonically advancing activation epoch.
    pub epoch: u64,
    /// Fresh nonce for this activation.
    pub nonce: Uuid,
}

/// A pinned installation anchor, independent of configuration and state roots.
pub struct Installation {
    path: PathBuf,
    id: Uuid,
    client: ClientAdapter,
    directory: PrivateDirectory,
}

impl Installation {
    /// Provision a new empty anchor and generate its lifetime UUID once.
    ///
    /// # Errors
    /// Refuses an existing, unsafe or unsupported anchor. Partial creation
    /// needs explicit managed repair or decommissioning.
    pub fn initialize(path: &Path, client: ClientAdapter) -> Result<Uuid> {
        let directory = PrivateDirectory::open(path, RootMode::CreateNew)?;
        let canonical =
            std::fs::canonicalize(path).map_err(|_| Error::InstallationInvalid("anchor path"))?;
        let id = Uuid::new_v4();
        let installation = Self {
            path: canonical,
            id,
            client,
            directory,
        };
        let _lock = installation.lock()?;
        let manifest = Manifest {
            version: 1,
            installation_id: id,
            anchor_path: installation.path.clone(),
            client,
            epoch: 0,
            nonce: Uuid::new_v4(),
            lifecycle: Lifecycle::Changing,
            config_path: None,
            fingerprint: None,
            snapshot_root: None,
            snapshot_root_device: None,
            snapshot_root_inode: None,
        };
        installation.transition(&manifest, TransitionKind::Activation)?;
        Ok(id)
    }

    /// Open an existing anchor pinned by the trusted launcher.
    ///
    /// # Errors
    /// Missing, corrupt, wrong-UUID and unsafe anchors fail without fallback.
    pub fn open(path: &Path, id: Uuid, client: ClientAdapter) -> Result<Self> {
        valid_path(path).map_err(|_| Error::InstallationInvalid("anchor path"))?;
        let directory = PrivateDirectory::open(path, RootMode::Existing)?;
        let canonical =
            std::fs::canonicalize(path).map_err(|_| Error::InstallationInvalid("anchor path"))?;
        let installation = Self {
            path: canonical,
            id,
            client,
            directory,
        };
        let _lock = installation.lock()?;
        installation.manifest()?;
        Ok(installation)
    }

    pub(crate) fn lock(&self) -> Result<File> {
        self.directory
            .lock(&format!("lock-installation-{}", self.id.simple()))
    }

    pub(crate) const fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) const fn client(&self) -> ClientAdapter {
        self.client
    }

    pub(crate) const fn directory(&self) -> &PrivateDirectory {
        &self.directory
    }

    pub(crate) fn epoch_locked(&self) -> Result<(u64, Uuid)> {
        let manifest = self.clean_manifest()?;
        if manifest.lifecycle != Lifecycle::Active {
            return Err(Error::InstallationInactive);
        }
        Ok((manifest.epoch, manifest.nonce))
    }

    fn manifest(&self) -> Result<Manifest> {
        let bytes = self
            .directory
            .read(MANIFEST)?
            .ok_or(Error::InstallationInvalid("missing manifest"))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|_| Error::InstallationInvalid("manifest"))?;
        if manifest.version != 1
            || manifest.installation_id != self.id
            || manifest.anchor_path != self.path
            || manifest.client != self.client
            || manifest.nonce.is_nil()
            || (manifest.lifecycle == Lifecycle::Active
                && (manifest.config_path.is_none()
                    || manifest.fingerprint.is_none()
                    || manifest.snapshot_root.is_none()
                    || manifest.snapshot_root_device.is_none()
                    || manifest.snapshot_root_inode.is_none()))
            || (manifest.lifecycle != Lifecycle::Active
                && (manifest.config_path.is_some()
                    || manifest.fingerprint.is_some()
                    || manifest.snapshot_root.is_some()
                    || manifest.snapshot_root_device.is_some()
                    || manifest.snapshot_root_inode.is_some()))
        {
            return Err(Error::InstallationInvalid("manifest identity or shape"));
        }
        Ok(manifest)
    }

    fn journal_unchecked(&self) -> Result<Journal> {
        let bytes = self
            .directory
            .read(JOURNAL)?
            .ok_or(Error::InstallationInvalid("missing journal"))?;
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| Error::InstallationInvalid("journal"))?;
        if journal.version != 1 || journal.nonce.is_nil() {
            return Err(Error::InstallationInvalid("journal version"));
        }
        Ok(journal)
    }

    fn journal(&self, manifest: &Manifest) -> Result<Journal> {
        let journal = self.journal_unchecked()?;
        if journal.epoch != manifest.epoch || journal.nonce != manifest.nonce {
            return Err(Error::InstallationInvalid("journal identity"));
        }
        Ok(journal)
    }

    fn clean_manifest(&self) -> Result<Manifest> {
        let manifest = self.manifest()?;
        if !self.journal(&manifest)?.resolved {
            return Err(Error::InstallationInactive);
        }
        Ok(manifest)
    }

    fn transition(&self, manifest: &Manifest, kind: TransitionKind) -> Result<()> {
        let journal = Journal {
            version: 1,
            epoch: manifest.epoch,
            nonce: manifest.nonce,
            kind,
            resolved: false,
        };
        let bytes = serde_json::to_vec(&journal)
            .map_err(|_| Error::InstallationInvalid("journal serialization"))?;
        self.directory.replace(JOURNAL, &bytes)?;
        let bytes = serde_json::to_vec(manifest)
            .map_err(|_| Error::InstallationInvalid("manifest serialization"))?;
        self.directory.replace(MANIFEST, &bytes)?;
        let bytes = serde_json::to_vec(&Journal {
            resolved: true,
            ..journal
        })
        .map_err(|_| Error::InstallationInvalid("journal serialization"))?;
        self.directory.replace(JOURNAL, &bytes)
    }

    fn next(&self, previous: &Manifest, lifecycle: Lifecycle) -> Result<Manifest> {
        let epoch = previous
            .epoch
            .checked_add(1)
            .ok_or(Error::InstallationInvalid("epoch overflow"))?;
        Ok(Manifest {
            version: 1,
            installation_id: self.id,
            anchor_path: self.path.clone(),
            client: self.client,
            epoch,
            nonce: Uuid::new_v4(),
            lifecycle,
            config_path: None,
            fingerprint: None,
            snapshot_root: None,
            snapshot_root_device: None,
            snapshot_root_inode: None,
        })
    }

    fn check_separation(&self, config: &TrustedHooksConfig) -> Result<()> {
        if within(&self.path, &config.state_root) || within(&config.state_root, &self.path) {
            return Err(Error::InstallationInvalid(
                "anchor and snapshot root overlap",
            ));
        }
        for binding in &config.bindings {
            let scope = match &binding.identity {
                BindingIdentity::Git { primary, common } => primary.as_ref().unwrap_or(common),
                BindingIdentity::Directory { root, .. } => root,
            };
            if within(&self.path, scope) || within(scope, &self.path) {
                return Err(Error::InstallationInvalid("anchor inside workspace"));
            }
        }
        Ok(())
    }

    /// Retire every prior head before validating a proposed v2 config/root.
    ///
    /// # Errors
    /// A failed validation leaves the installation Changing at the new epoch.
    pub fn activate(&self, proposed_config: &Path) -> Result<u64> {
        let _lock = self.lock()?;
        let old = self.clean_manifest()?;
        if old.lifecycle == Lifecycle::Retired {
            return Err(Error::InstallationInactive);
        }
        let mut changing = self.next(&old, Lifecycle::Changing)?;
        self.transition(&changing, TransitionKind::Activation)?;
        let config = TrustedHooksConfig::load(proposed_config)?;
        let delivery = config
            .delivery()
            .ok_or(Error::InstallationInvalid("v2 config required"))?;
        if delivery.client().adapter() != self.client {
            return Err(Error::InstallationInvalid("client adapter"));
        }
        self.check_separation(&config)?;
        let root = PrivateDirectory::open(&config.state_root, RootMode::CreateOrOpen)?;
        let (device, inode) = root.identity()?;
        changing.lifecycle = Lifecycle::Active;
        changing.config_path = Some(config.config_path);
        changing.fingerprint = Some(config.fingerprint);
        changing.snapshot_root = Some(config.state_root);
        changing.snapshot_root_device = Some(device);
        changing.snapshot_root_inode = Some(inode);
        self.transition(&changing, TransitionKind::Activation)?;
        Ok(changing.epoch)
    }

    /// Durably retire this anchor; no future activation is accepted.
    ///
    /// # Errors
    /// Fails when the manifest or journal is corrupt or state durability fails.
    pub fn retire(&self) -> Result<u64> {
        let _lock = self.lock()?;
        let old = self.clean_manifest()?;
        if old.lifecycle == Lifecycle::Retired {
            return Err(Error::InstallationInactive);
        }
        let retired = self.next(&old, Lifecycle::Retired)?;
        self.transition(&retired, TransitionKind::Retirement)?;
        Ok(retired.epoch)
    }

    /// Repair an unresolved transition by advancing, never restoring, its epoch.
    ///
    /// # Errors
    /// A corrupt manifest or journal requires managed decommissioning.
    pub fn repair_incomplete(&self) -> Result<u64> {
        let _lock = self.lock()?;
        let old = self.manifest()?;
        let journal = self.journal_unchecked()?;
        if journal.resolved {
            return Err(Error::InstallationInvalid("no incomplete transition"));
        }
        if journal.epoch < old.epoch
            || journal.epoch > old.epoch.saturating_add(1)
            || (journal.epoch == old.epoch && journal.nonce != old.nonce)
        {
            return Err(Error::InstallationInvalid("journal identity"));
        }
        let lifecycle = match journal.kind {
            TransitionKind::Activation => Lifecycle::Changing,
            TransitionKind::Retirement => Lifecycle::Retired,
        };
        let mut recovered = self.next(&old, lifecycle)?;
        recovered.epoch = journal
            .epoch
            .max(old.epoch)
            .checked_add(1)
            .ok_or(Error::InstallationInvalid("epoch overflow"))?;
        self.transition(&recovered, journal.kind)?;
        Ok(recovered.epoch)
    }

    fn invalidate_mismatch(&self, current: &Manifest) -> Result<()> {
        let changing = self.next(current, Lifecycle::Changing)?;
        self.transition(&changing, TransitionKind::Activation)
    }

    /// Reload the active protected config and root under the fixed lock.
    ///
    /// A mismatch commits a new Changing epoch, so reverting edited bytes
    /// cannot resurrect earlier heads. Callers must separately lock a session.
    ///
    /// # Errors
    /// Missing, retired, unresolved or mismatched authority is unusable.
    pub fn active(&self) -> Result<ActiveInstallation> {
        let _lock = self.lock()?;
        self.active_locked()
    }

    pub(crate) fn active_locked(&self) -> Result<ActiveInstallation> {
        let manifest = self.clean_manifest()?;
        if manifest.lifecycle != Lifecycle::Active {
            return Err(Error::InstallationInactive);
        }
        let path = manifest
            .config_path
            .as_deref()
            .ok_or(Error::InstallationInvalid("active config path"))?;
        let config = TrustedHooksConfig::load(path);
        let valid = config.as_ref().is_ok_and(|config| {
            manifest.fingerprint.as_deref() == Some(config.fingerprint())
                && manifest.snapshot_root.as_deref() == Some(config.state_root.as_path())
                && config
                    .delivery()
                    .is_some_and(|delivery| delivery.client().adapter() == self.client)
                && self.check_separation(config).is_ok()
                && PrivateDirectory::open(&config.state_root, RootMode::Existing)
                    .and_then(|root| root.identity())
                    .is_ok_and(|(device, inode)| {
                        manifest.snapshot_root_device == Some(device)
                            && manifest.snapshot_root_inode == Some(inode)
                    })
        });
        if !valid {
            self.invalidate_mismatch(&manifest)?;
            return Err(Error::InstallationMismatch);
        }
        Ok(ActiveInstallation {
            config: config.map_err(|_| Error::InstallationMismatch)?,
            epoch: manifest.epoch,
            nonce: manifest.nonce,
        })
    }

    /// Read the current lifecycle and epoch for administrative inspection.
    ///
    /// # Errors
    /// Corrupt or unresolved control records fail without fallback.
    pub fn status(&self) -> Result<(Lifecycle, u64)> {
        let _lock = self.lock()?;
        let manifest = self.clean_manifest()?;
        Ok((manifest.lifecycle, manifest.epoch))
    }
}
