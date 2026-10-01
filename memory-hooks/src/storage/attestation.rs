//! Protected provisioning assertion; never infer authority from runtime facts.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{Binding, BindingIdentity, valid_path};
use crate::error::{Error, Result};

/// Raw pins are private and deliberately have no Debug implementation.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StorageExecution {
    contract: String,
    pub(crate) binding_root: PathBuf,
    pub(crate) project: String,
    pub(crate) boot_id: String,
    pub(crate) namespace_device: u64,
    pub(crate) namespace_inode: u64,
    pub(crate) root_device: u64,
    pub(crate) root_inode: u64,
    pub(crate) isolated_woodpecker: bool,
    pub(crate) mounts: Vec<LocalMount>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalMount {
    pub(crate) id: u64,
    pub(crate) root: PathBuf,
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) mountpoint: PathBuf,
    pub(crate) mountpoint_device: u64,
    pub(crate) mountpoint_inode: u64,
    pub(crate) filesystem: String,
    pub(crate) provenance: String,
}

impl StorageExecution {
    pub(crate) fn validate(&self, bindings: &[Binding]) -> Result<()> {
        valid_path(&self.binding_root)?;
        let boot = uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|_| Error::ConfigInvalid("storage boot pin"))?;
        if self.contract != "isolated-woodpecker-storage-v1"
            || !self.isolated_woodpecker
            || boot.is_nil()
            || self.boot_id != boot.to_string()
            || self.namespace_inode == 0
            || self.root_inode == 0
            || self.mounts.is_empty()
            || self.mounts.len() > 64
        {
            return Err(Error::ConfigInvalid("storage execution pins"));
        }
        let matches = bindings
            .iter()
            .filter(|binding| {
                let root = match &binding.identity {
                    BindingIdentity::Git { common, .. } => common,
                    BindingIdentity::Directory { root, .. } => root,
                };
                root == &self.binding_root
                    && binding.project == self.project
                    && binding.context.profile.as_deref() == Some("woodpecker-container")
            })
            .count();
        if matches != 1 {
            return Err(Error::ConfigInvalid("storage binding"));
        }
        let mut seen = BTreeSet::new();
        for mount in &self.mounts {
            valid_path(&mount.root)?;
            valid_path(&mount.mountpoint)?;
            if mount.id == 0
                || mount.mountpoint_inode == 0
                || !seen.insert(mount.id)
                || !matches!(
                    mount.filesystem.as_str(),
                    "tmpfs" | "ramfs" | "overlay" | "ext2" | "ext3" | "ext4" | "xfs" | "btrfs"
                )
                || mount.provenance != "container-local"
            {
                return Err(Error::ConfigInvalid("storage local mount"));
            }
        }
        Ok(())
    }
}
