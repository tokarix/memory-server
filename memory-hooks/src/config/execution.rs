//! Protected Rust execution declarations; runtime data cannot create authority.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    Binding, BindingIdentity, ClientAdapter, hash_part, open_trusted_path, valid_path, within,
};
use crate::error::{Error, Result};
use crate::limits;

/// Versioned, validated operator promise for a sealed local client launcher.
/// Private paths and environment values deliberately have no Debug/Serialize.
#[derive(Clone)]
pub struct RustExecution {
    raw: RawRustExecution,
    identities: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRustExecution {
    contract: String,
    client_pin: String,
    semantics: String,
    shell_mode: String,
    locality: String,
    tool_resolution: String,
    inherited_environment: String,
    client_ancestor: u8,
    boot_id: String,
    namespace_device: u64,
    namespace_inode: u64,
    root_device: u64,
    root_inode: u64,
    toolchain: String,
    executables: BTreeMap<String, PathBuf>,
    environment: BTreeMap<String, String>,
}

/// Only these variables can affect the supported static Rust contract.
/// Other Cargo/Rust variables and dynamic-loader/shell mechanisms deny.
#[must_use]
pub fn supported_environment_key(key: &str) -> bool {
    matches!(
        key,
        "HOME"
            | "PATH"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "RUSTUP_TOOLCHAIN"
            | "CARGO_TARGET_DIR"
            | "CARGO_BUILD_TARGET_DIR"
            | "CARGO_BUILD_BUILD_DIR"
            | "TMPDIR"
            | "TMP"
            | "TEMP"
            | "CARGO_INSTALL_ROOT"
            | "RUSTC"
            | "RUSTDOC"
            | "RUSTC_WRAPPER"
            | "RUSTC_WORKSPACE_WRAPPER"
            | "RUSTFLAGS"
            | "CARGO_ENCODED_RUSTFLAGS"
            | "RUSTDOCFLAGS"
            | "CARGO_ENCODED_RUSTDOCFLAGS"
            | "CARGO_BUILD_RUSTFLAGS"
            | "LANG"
            | "LC_ALL"
            | "TERM"
            | "NO_COLOR"
    )
}

fn executable_stamp(path: &Path, deadline: Instant) -> Result<Vec<u8>> {
    let mut file = open_trusted_path(path, true)?;
    let meta = file
        .metadata()
        .map_err(|_| Error::ConfigUntrusted("Rust executable"))?;
    if meta.len() > 256 * 1024 * 1024 {
        return Err(Error::ConfigInvalid("Rust executable size"));
    }
    let mut hash = Sha256::new();
    for value in [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime().cast_unsigned(),
        meta.mtime_nsec().cast_unsigned(),
        meta.ctime().cast_unsigned(),
        meta.ctime_nsec().cast_unsigned(),
    ] {
        hash.update(value.to_be_bytes());
    }
    let mut block = vec![0_u8; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(Error::ConfigUntrusted("Rust executable deadline"));
        }
        let count = file
            .read(&mut block)
            .map_err(|_| Error::ConfigUntrusted("Rust executable read"))?;
        if count == 0 {
            break;
        }
        hash.update(&block[..count]);
    }
    Ok(hash.finalize().to_vec())
}

fn read_proc(path: &Path, deadline: Instant) -> Result<Vec<u8>> {
    if Instant::now() >= deadline {
        return Err(Error::ConfigUntrusted("Rust locality deadline"));
    }
    let file = File::open(path).map_err(|_| Error::ConfigUntrusted("Rust locality read"))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::ConfigUntrusted("Rust locality read"))?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::ConfigUntrusted("Rust locality size"));
    }
    Ok(bytes)
}

fn process_stamp(bytes: &[u8]) -> Result<(u32, u64)> {
    let stat =
        std::str::from_utf8(bytes).map_err(|_| Error::ConfigUntrusted("Rust process encoding"))?;
    let tail = stat
        .rsplit_once(") ")
        .ok_or(Error::ConfigUntrusted("Rust process identity"))?
        .1;
    let words: Vec<_> = tail.split_ascii_whitespace().collect();
    let parent = words
        .get(1)
        .ok_or(Error::ConfigUntrusted("Rust process parent"))?
        .parse()
        .map_err(|_| Error::ConfigUntrusted("Rust process parent"))?;
    let started = words
        .get(19)
        .ok_or(Error::ConfigUntrusted("Rust process start"))?
        .parse()
        .map_err(|_| Error::ConfigUntrusted("Rust process start"))?;
    Ok((parent, started))
}

impl RawRustExecution {
    fn validate_environment(&self) -> Result<()> {
        for (key, value) in &self.environment {
            if !supported_environment_key(key)
                || value.len() > limits::PATH_BYTES
                || value.contains('\0')
            {
                return Err(Error::ConfigInvalid("Rust inherited environment"));
            }
        }
        for key in ["HOME", "PATH", "CARGO_HOME", "RUSTUP_HOME", "TMPDIR"] {
            let value = self
                .environment
                .get(key)
                .ok_or(Error::ConfigInvalid("Rust inherited environment inventory"))?;
            if key != "PATH" {
                valid_path(Path::new(value))?;
            }
        }
        let search = self
            .environment
            .get("PATH")
            .ok_or(Error::ConfigInvalid("Rust PATH inventory"))?;
        for directory in search.split(':') {
            valid_path(Path::new(directory))?;
        }
        for (key, tool) in [("RUSTC", "rustc"), ("RUSTDOC", "rustdoc")] {
            if self
                .environment
                .get(key)
                .is_some_and(|value| self.executables.get(tool) != Some(&PathBuf::from(value)))
            {
                return Err(Error::ConfigInvalid("Rust compiler override"));
            }
        }
        for key in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
            if self
                .environment
                .get(key)
                .is_some_and(|value| !value.is_empty())
            {
                return Err(Error::ConfigInvalid("Rust compiler wrapper"));
            }
        }
        if self
            .environment
            .get("RUSTUP_TOOLCHAIN")
            .is_some_and(|value| value != &self.toolchain)
        {
            return Err(Error::ConfigInvalid("Rust toolchain environment"));
        }
        Ok(())
    }

    pub(super) fn validate(
        self,
        adapter: ClientAdapter,
        bindings: &[Binding],
    ) -> Result<RustExecution> {
        let client = match adapter {
            ClientAdapter::CodexV1 => "codex-cli-0.158.0",
            ClientAdapter::ClaudeV1 => "claude-code-2.1.92",
        };
        let boot = uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|_| Error::ConfigInvalid("Rust boot pin"))?;
        if self.contract != "managed-rust-execution-v1"
            || self.client_pin != client
            || self.semantics != "rust-cargo-1.94.0-linux-v1"
            || self.shell_mode != "posix-literal-no-startup-v1"
            || self.locality != "same-mount-namespace-and-root-v1"
            || self.tool_resolution != "sealed-executables-v1"
            || self.inherited_environment != "sealed-environment-v1"
            || !(1..=4).contains(&self.client_ancestor)
            || boot.is_nil()
            || boot.to_string() != self.boot_id
            || self.namespace_inode == 0
            || self.root_inode == 0
            || self.toolchain != "1.94.0-x86_64-unknown-linux-gnu"
            || self.environment.len() > limits::EXECUTION_ENVIRONMENT
            || self.executables.len() != 9
        {
            return Err(Error::ConfigInvalid("Rust execution contract"));
        }
        for name in [
            "client",
            "shell",
            "env",
            "cargo",
            "rustc",
            "rustdoc",
            "rustup",
            "cargo-clippy",
            "clippy-driver",
        ] {
            if !self.executables.contains_key(name) {
                return Err(Error::ConfigInvalid("Rust executable inventory"));
            }
        }
        self.validate_environment()?;
        let deadline = Instant::now() + limits::RESOLVE_DEADLINE;
        let mut identities = BTreeMap::new();
        for (name, path) in &self.executables {
            valid_path(path)?;
            for binding in bindings {
                let scope = match &binding.identity {
                    BindingIdentity::Git { common, primary } => primary.as_ref().unwrap_or(common),
                    BindingIdentity::Directory { root, .. } => root,
                };
                if within(path, scope) {
                    return Err(Error::ConfigUntrusted("Rust executable inside workspace"));
                }
            }
            identities.insert(name.clone(), executable_stamp(path, deadline)?);
        }
        Ok(RustExecution {
            raw: self,
            identities,
        })
    }
}

impl RustExecution {
    /// Verify the managed client's observable process identity and locality.
    /// The protected launcher declaration supplies the sealed shell semantics;
    /// ambient hook variables and the event cannot supply that declaration.
    ///
    /// # Errors
    /// Missing or changed process, namespace, root or environment evidence denies.
    pub fn verify_locality(&self, deadline: Instant) -> Result<()> {
        let boot = read_proc(Path::new("/proc/sys/kernel/random/boot_id"), deadline)?;
        if boot.strip_suffix(b"\n") != Some(self.raw.boot_id.as_bytes()) {
            return Err(Error::ConfigUntrusted("Rust boot changed"));
        }
        let mut pid = std::process::id();
        for _ in 0..self.raw.client_ancestor {
            let stat = read_proc(&PathBuf::from(format!("/proc/{pid}/stat")), deadline)?;
            let stat = std::str::from_utf8(&stat)
                .map_err(|_| Error::ConfigUntrusted("Rust client identity"))?;
            let tail = stat
                .rsplit_once(") ")
                .ok_or(Error::ConfigUntrusted("Rust client identity"))?
                .1;
            pid = tail
                .split_ascii_whitespace()
                .nth(1)
                .ok_or(Error::ConfigUntrusted("Rust client identity"))?
                .parse()
                .map_err(|_| Error::ConfigUntrusted("Rust client identity"))?;
            if pid == 0 {
                return Err(Error::ConfigUntrusted("Rust client missing"));
            }
        }
        let client = PathBuf::from(format!("/proc/{pid}"));
        let before = process_stamp(&read_proc(&client.join("stat"), deadline)?)?;
        let executable = std::fs::read_link(client.join("exe"))
            .map_err(|_| Error::ConfigUntrusted("Rust client executable"))?;
        if self.raw.executables.get("client") != Some(&executable) {
            return Err(Error::ConfigUntrusted("Rust client executable mismatch"));
        }
        let running = std::fs::metadata(client.join("exe"))
            .map_err(|_| Error::ConfigUntrusted("Rust running executable"))?;
        let pinned = std::fs::metadata(&executable)
            .map_err(|_| Error::ConfigUntrusted("Rust pinned executable"))?;
        if running.dev() != pinned.dev()
            || running.ino() != pinned.ino()
            || running.len() != pinned.len()
        {
            return Err(Error::ConfigUntrusted("Rust running executable replaced"));
        }
        for (suffix, device, inode) in [
            (
                "ns/mnt",
                self.raw.namespace_device,
                self.raw.namespace_inode,
            ),
            ("root", self.raw.root_device, self.raw.root_inode),
        ] {
            for prefix in [&client, &PathBuf::from("/proc/self")] {
                let metadata = std::fs::metadata(prefix.join(suffix))
                    .map_err(|_| Error::ConfigUntrusted("Rust locality identity"))?;
                if metadata.dev() != device || metadata.ino() != inode {
                    return Err(Error::ConfigUntrusted("Rust locality mismatch"));
                }
            }
        }
        let bytes = read_proc(&client.join("environ"), deadline)?;
        let mut environment = BTreeMap::new();
        for item in bytes
            .split(|byte| *byte == 0)
            .filter(|item| !item.is_empty())
        {
            let item = std::str::from_utf8(item)
                .map_err(|_| Error::ConfigUntrusted("Rust client environment"))?;
            let (key, value) = item
                .split_once('=')
                .ok_or(Error::ConfigUntrusted("Rust client environment"))?;
            if supported_environment_key(key) {
                if environment
                    .insert(key.to_owned(), value.to_owned())
                    .is_some()
                {
                    return Err(Error::ConfigUntrusted("Rust environment duplicate"));
                }
            } else if ["CARGO_", "RUST", "LD_", "DYLD_"]
                .iter()
                .any(|prefix| key.starts_with(prefix))
                || matches!(key, "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS")
                || key.starts_with("BASH_FUNC_")
            {
                return Err(Error::ConfigUntrusted("Rust environment mechanism"));
            }
        }
        if environment != self.raw.environment
            || process_stamp(&read_proc(&client.join("stat"), deadline)?)? != before
        {
            return Err(Error::ConfigUntrusted("Rust execution evidence changed"));
        }
        self.recheck_executables(deadline)
    }

    /// Recheck the protected executable inventory without launching any tool.
    ///
    /// # Errors
    /// Changed, inaccessible or late executable evidence is unsupported.
    pub fn recheck_executables(&self, deadline: Instant) -> Result<()> {
        for (name, path) in &self.raw.executables {
            if self.identities.get(name) != Some(&executable_stamp(path, deadline)?) {
                return Err(Error::ConfigUntrusted("Rust executable changed"));
            }
        }
        Ok(())
    }

    /// Fixed grammar/semantic version, suitable for safe audit metadata.
    #[must_use]
    pub const fn version(&self) -> u8 {
        1
    }

    /// Resolve a literal bare tool name or exact protected absolute spelling.
    #[must_use]
    pub fn tool(&self, word: &str) -> Option<&str> {
        self.raw.executables.iter().find_map(|(name, path)| {
            let bare = word == name
                && self
                    .raw
                    .environment
                    .get("PATH")
                    .and_then(|search| {
                        search
                            .split(':')
                            .map(|directory| Path::new(directory).join(name))
                            .find(|candidate| candidate.exists())
                    })
                    .as_ref()
                    == Some(path);
            ((bare || Path::new(word) == path) && !matches!(name.as_str(), "client" | "shell"))
                .then_some(name.as_str())
        })
    }

    /// Already installed operator-pinned toolchain. No discovery is executed.
    #[must_use]
    pub fn toolchain(&self) -> &str {
        &self.raw.toolchain
    }

    /// Sealed inherited build environment; callers must keep values private.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.raw.environment
    }

    pub(super) fn fingerprint(&self, hash: &mut Sha256) {
        hash_part(hash, b"managed-rust-execution-v1");
        for value in [
            &self.raw.client_pin,
            &self.raw.semantics,
            &self.raw.shell_mode,
            &self.raw.locality,
            &self.raw.tool_resolution,
            &self.raw.inherited_environment,
            &self.raw.boot_id,
            &self.raw.toolchain,
        ] {
            hash_part(hash, value.as_bytes());
        }
        hash_part(hash, &[self.raw.client_ancestor]);
        for value in [
            self.raw.namespace_device,
            self.raw.namespace_inode,
            self.raw.root_device,
            self.raw.root_inode,
        ] {
            hash_part(hash, &value.to_be_bytes());
        }
        for (name, path) in &self.raw.executables {
            hash_part(hash, name.as_bytes());
            hash_part(hash, super::path_bytes(path));
        }
        for (name, identity) in &self.identities {
            hash_part(hash, name.as_bytes());
            hash_part(hash, identity);
        }
        for (key, value) in &self.raw.environment {
            hash_part(hash, key.as_bytes());
            hash_part(hash, value.as_bytes());
        }
    }
}
