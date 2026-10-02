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
    installed_identities: BTreeMap<String, Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{executable_stamp, literal_fixture, unsupported_environment_mechanism};

    #[test]
    fn installed_secondary_identity_rechecks_never_execute_tools() {
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let directory = tempfile::tempdir_in(base).unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let (mut contract, _parser_directory) = literal_fixture();
        let deadline = Instant::now() + Duration::from_secs(5);
        for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\nexit 99\nNONEXECUTION_SENTINEL\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            contract
                .raw
                .installed_executables
                .insert(name.to_owned(), path.clone());
            contract
                .installed_identities
                .insert(name.to_owned(), executable_stamp(&path, deadline).unwrap());
        }
        contract
            .verify_installed_toolchain(directory.path(), deadline)
            .unwrap();
        assert!(
            contract
                .verify_installed_toolchain(
                    directory.path(),
                    Instant::now().checked_sub(Duration::from_secs(1)).unwrap()
                )
                .is_err()
        );
        fs::write(bin.join("rustc"), "REPLACED_SECRET_SENTINEL").unwrap();
        let error = contract
            .verify_installed_toolchain(directory.path(), deadline)
            .unwrap_err();
        assert!(!format!("{error:?}").contains("REPLACED_SECRET_SENTINEL"));
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn protected_executable_hash_rejects_fifo_device_and_symlink_before_read_open() {
        use std::os::unix::fs::symlink;

        use rustix::fs::{Mode, mkfifoat};

        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let directory = tempfile::tempdir_in(base).unwrap();
        let fifo = directory.path().join("compiler-secret-sentinel");
        mkfifoat(rustix::fs::CWD, &fifo, Mode::from_bits_truncate(0o700)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        for path in [&fifo, &PathBuf::from("/dev/null")] {
            let error = executable_stamp(path, deadline).unwrap_err();
            assert!(!format!("{error:?}").contains("compiler-secret-sentinel"));
        }
        let alias = directory.path().join("alias");
        symlink(&fifo, &alias).unwrap();
        assert!(executable_stamp(&alias, deadline).is_err());
        assert!(Instant::now() < deadline);
    }

    #[test]
    fn ambient_secondary_and_shell_mechanisms_cannot_hide_outside_inventory() {
        for key in [
            "CARGO",
            "CARGO_ALIAS_B",
            "RUSTFLAGS",
            "RUSTUP_TOOLCHAIN",
            "CLIPPY_ARGS",
            "CLIPPY_CONF_DIR",
            "SYSROOT",
            "LD_PRELOAD",
            "BASH_FUNC_cargo%%",
            "BASH_ENV",
            "ENV",
        ] {
            assert!(unsupported_environment_mechanism(key));
        }
        for key in ["SSH_AUTH_SOCK", "DISPLAY", "USER"] {
            assert!(!unsupported_environment_mechanism(key));
        }
    }
}

#[cfg(test)]
pub(crate) fn literal_fixture() -> (RustExecution, tempfile::TempDir) {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let directory = tempfile::tempdir_in(base).unwrap();
    let executables: BTreeMap<_, _> = [
        "client",
        "shell",
        "env",
        "cargo",
        "rustc",
        "rustdoc",
        "rustup",
        "cargo-clippy",
        "clippy-driver",
    ]
    .into_iter()
    .map(|name| {
        let path = directory.path().join(name);
        std::fs::write(&path, "NONEXECUTION_SENTINEL").unwrap();
        (name.to_owned(), path)
    })
    .collect();
    let environment = BTreeMap::from([
        (
            "PATH".to_owned(),
            directory.path().to_str().unwrap().to_owned(),
        ),
        ("HOME".to_owned(), "/sealed/home".to_owned()),
        ("CARGO_HOME".to_owned(), "/sealed/cargo-home".to_owned()),
        ("RUSTUP_HOME".to_owned(), "/sealed/rustup".to_owned()),
        ("TMPDIR".to_owned(), "/sealed/tmp".to_owned()),
    ]);
    (
        RustExecution {
            raw: RawRustExecution {
                contract: "managed-rust-execution-v1".to_owned(),
                client_pin: "codex-cli-0.158.0".to_owned(),
                semantics: "rust-cargo-1.94.0-linux-v1".to_owned(),
                rustup_semantics: "rustup-1.28.2-linux-v1".to_owned(),
                shell_mode: "posix-literal-no-startup-v1".to_owned(),
                locality: "same-mount-namespace-and-root-v1".to_owned(),
                tool_resolution: "sealed-executables-v2".to_owned(),
                inherited_environment: "sealed-environment-v1".to_owned(),
                client_ancestor: 3,
                boot_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                namespace_device: 1,
                namespace_inode: 1,
                root_device: 1,
                root_inode: 1,
                toolchain: "1.94.0-x86_64-unknown-linux-gnu".to_owned(),
                executables,
                installed_executables: [
                    "cargo",
                    "rustc",
                    "rustdoc",
                    "cargo-clippy",
                    "clippy-driver",
                ]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        PathBuf::from(
                            "/sealed/rustup/toolchains/1.94.0-x86_64-unknown-linux-gnu/bin",
                        )
                        .join(name),
                    )
                })
                .collect(),
                environment,
            },
            identities: BTreeMap::new(),
            installed_identities: BTreeMap::new(),
        },
        directory,
    )
}

#[cfg(test)]
pub(crate) fn execution_fixture(proxy: bool) -> (RustExecution, tempfile::TempDir) {
    use std::os::unix::fs::PermissionsExt;

    let (mut contract, directory) = literal_fixture();
    let home = directory.path().join("cargo-home");
    let rustup = directory.path().join("rustup-home");
    let installed = rustup.join("toolchains").join(contract.toolchain());
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::create_dir_all(installed.join("bin")).unwrap();
    std::fs::create_dir(installed.join("lib")).unwrap();
    for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
        let path = installed.join("bin").join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nexit 99\nINSTALLED_NONEXECUTION_SENTINEL\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        contract
            .raw
            .installed_executables
            .insert(name.to_owned(), path.clone());
        let primary = if proxy {
            let primary = home.join("bin").join(name);
            std::fs::write(
                &primary,
                "#!/bin/sh\nexit 99\nPROXY_NONEXECUTION_SENTINEL\n",
            )
            .unwrap();
            std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o700)).unwrap();
            primary
        } else {
            path
        };
        contract.raw.executables.insert(name.to_owned(), primary);
    }
    contract
        .raw
        .environment
        .insert("CARGO_HOME".to_owned(), home.to_str().unwrap().to_owned());
    contract.raw.environment.insert(
        "RUSTUP_HOME".to_owned(),
        rustup.to_str().unwrap().to_owned(),
    );
    let empty = directory.path().join("search-empty");
    std::fs::create_dir(&empty).unwrap();
    contract.raw.environment.insert(
        "PATH".to_owned(),
        format!(
            "{}:{}:{}",
            empty.display(),
            if proxy {
                home.join("bin")
            } else {
                installed.join("bin")
            }
            .display(),
            directory.path().display()
        ),
    );
    let deadline = Instant::now() + limits::RESOLVE_DEADLINE;
    let marker = directory
        .path()
        .join("marker")
        .to_str()
        .unwrap()
        .replace('\'', "'\\''");
    let script = format!("#!/bin/sh\n: > '{marker}'\nexit 99\n# NONEXECUTION_SENTINEL\n");
    for path in contract
        .raw
        .executables
        .values()
        .chain(contract.raw.installed_executables.values())
    {
        std::fs::write(path, &script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for (inventory, identities) in [
        (&contract.raw.executables, &mut contract.identities),
        (
            &contract.raw.installed_executables,
            &mut contract.installed_identities,
        ),
    ] {
        for (name, path) in inventory {
            identities.insert(name.clone(), executable_stamp(path, deadline).unwrap());
        }
    }
    (contract, directory)
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRustExecution {
    contract: String,
    client_pin: String,
    semantics: String,
    rustup_semantics: String,
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
    installed_executables: BTreeMap<String, PathBuf>,
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
            | "CARGO_BUILD_TARGET"
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

fn unsupported_environment_mechanism(key: &str) -> bool {
    ["CARGO_", "RUST", "CLIPPY_", "LD_", "DYLD_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || matches!(
            key,
            "CARGO" | "SYSROOT" | "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS"
        )
        || key.starts_with("BASH_FUNC_")
}

fn metadata_stamp(meta: &std::fs::Metadata) -> [u64; 10] {
    [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime().cast_unsigned(),
        meta.mtime_nsec().cast_unsigned(),
        meta.ctime().cast_unsigned(),
        meta.ctime_nsec().cast_unsigned(),
        u64::from(meta.mode()),
        u64::from(meta.uid()),
        u64::from(meta.gid()),
    ]
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
    for value in metadata_stamp(&meta) {
        hash.update(value.to_be_bytes());
    }
    let mut block = vec![0_u8; 64 * 1024];
    let mut bytes_read = 0_u64;
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
        bytes_read +=
            u64::try_from(count).map_err(|_| Error::ConfigUntrusted("Rust executable size"))?;
        if bytes_read > meta.len() {
            return Err(Error::ConfigUntrusted("Rust executable grew during read"));
        }
        hash.update(&block[..count]);
    }
    if bytes_read != meta.len()
        || metadata_stamp(
            &file
                .metadata()
                .map_err(|_| Error::ConfigUntrusted("Rust executable recheck"))?,
        ) != metadata_stamp(&meta)
        || Instant::now() >= deadline
    {
        return Err(Error::ConfigUntrusted(
            "Rust executable changed during read",
        ));
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

    fn validate_mappings(&self) -> Result<()> {
        let installed = Path::new(
            self.environment
                .get("RUSTUP_HOME")
                .ok_or(Error::ConfigInvalid("Rust installed home"))?,
        )
        .join("toolchains")
        .join(&self.toolchain)
        .join("bin");
        let proxies = Path::new(
            self.environment
                .get("CARGO_HOME")
                .ok_or(Error::ConfigInvalid("Rust proxy home"))?,
        )
        .join("bin");
        let proxy = self.executables.get("cargo") != self.installed_executables.get("cargo");
        for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
            let path = self
                .installed_executables
                .get(name)
                .ok_or(Error::ConfigInvalid("Rust installed inventory"))?;
            if *path != installed.join(name)
                || self.executables.get(name)
                    != Some(&if proxy {
                        proxies.join(name)
                    } else {
                        path.clone()
                    })
            {
                return Err(Error::ConfigInvalid("Rust installed mapping"));
            }
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
            || self.rustup_semantics != "rustup-1.28.2-linux-v1"
            || self.shell_mode != "posix-literal-no-startup-v1"
            || self.locality != "same-mount-namespace-and-root-v1"
            || self.tool_resolution != "sealed-executables-v2"
            || self.inherited_environment != "sealed-environment-v1"
            || !(1..=4).contains(&self.client_ancestor)
            || boot.is_nil()
            || boot.to_string() != self.boot_id
            || self.namespace_inode == 0
            || self.root_inode == 0
            || self.toolchain != "1.94.0-x86_64-unknown-linux-gnu"
            || self.environment.len() > limits::EXECUTION_ENVIRONMENT
            || self.executables.len() != 9
            || self.installed_executables.len() != 5
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
        self.validate_mappings()?;
        let deadline = Instant::now() + limits::RESOLVE_DEADLINE;
        let mut identities = BTreeMap::new();
        let mut installed_identities = BTreeMap::new();
        for (inventory, stamps) in [
            (&self.executables, &mut identities),
            (&self.installed_executables, &mut installed_identities),
        ] {
            for (name, path) in inventory {
                valid_path(path)?;
                for binding in bindings {
                    let scope = match &binding.identity {
                        BindingIdentity::Git { common, primary } => {
                            primary.as_ref().unwrap_or(common)
                        }
                        BindingIdentity::Directory { root, .. } => root,
                    };
                    if within(path, scope) {
                        return Err(Error::ConfigUntrusted("Rust executable inside workspace"));
                    }
                }
                stamps.insert(name.clone(), executable_stamp(path, deadline)?);
            }
        }
        Ok(RustExecution {
            raw: self,
            identities,
            installed_identities,
        })
    }
}

impl RustExecution {
    /// Whether a client-specified shell is the exact protected executable.
    #[must_use]
    pub fn accepts_shell(&self, path: &Path) -> bool {
        self.raw
            .executables
            .get("shell")
            .is_some_and(|shell| path == shell)
    }

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
            } else if unsupported_environment_mechanism(key) {
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

    /// Verify that rustup's installed secondary tools have the protected identities.
    /// This reads trusted regular files; no proxy or compiler is executed.
    ///
    /// # Errors
    /// Missing tools, proxy/replacement identities and late evidence deny.
    pub fn verify_installed_toolchain(&self, directory: &Path, deadline: Instant) -> Result<()> {
        valid_path(directory)?;
        for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
            let path = directory.join("bin").join(name);
            valid_path(&path)?;
            if self.raw.installed_executables.get(name) != Some(&path)
                || self.installed_identities.get(name) != Some(&executable_stamp(&path, deadline)?)
            {
                return Err(Error::ConfigUntrusted("Rust installed executable mismatch"));
            }
        }
        Ok(())
    }

    /// Fixed grammar/semantic version, suitable for safe audit metadata.
    #[must_use]
    pub const fn version(&self) -> u8 {
        2
    }

    /// Private exact entrypoint path, separate from rustup's installed tools.
    #[must_use]
    pub fn executable(&self, name: &str) -> Option<&Path> {
        self.raw.executables.get(name).map(PathBuf::as_path)
    }

    /// Private installed secondary path; it never participates in bare-name
    /// primary resolution unless explicitly declared as that entrypoint.
    #[must_use]
    pub fn installed_executable(&self, name: &str) -> Option<&Path> {
        self.raw
            .installed_executables
            .get(name)
            .map(PathBuf::as_path)
    }

    /// Whether a protected primary entrypoint is a rustup proxy.
    #[must_use]
    pub fn is_proxy(&self, name: &str) -> bool {
        self.raw
            .installed_executables
            .get(name)
            .is_some_and(|installed| {
                self.raw
                    .executables
                    .get(name)
                    .is_some_and(|entry| entry != installed)
            })
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
            &self.raw.rustup_semantics,
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
        hash_part(hash, b"installed-executables-v1");
        for (name, path) in &self.raw.installed_executables {
            hash_part(hash, name.as_bytes());
            hash_part(hash, super::path_bytes(path));
        }
        for (name, identity) in &self.installed_identities {
            hash_part(hash, name.as_bytes());
            hash_part(hash, identity);
        }
        for (key, value) in &self.raw.environment {
            hash_part(hash, key.as_bytes());
            hash_part(hash, value.as_bytes());
        }
    }
}
