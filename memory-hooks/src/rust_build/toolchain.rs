//! Bounded rustup selector evidence, separate from executable/locality authority.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use toml::Value;

use crate::config::execution::RustExecution;
use crate::limits;

use super::command::Invocation;
use super::environment::Environment;
use super::files::{MAX_DEPTH, ReadProvider, ReadSet};
use super::{Failure, paths};

const HOST: &str = "x86_64-unknown-linux-gnu";

/// Safe selection provenance; neither selector values nor paths are exposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    /// Parsed explicit Cargo selector or rustup run wrapper.
    Command,
    /// Effective process `RUSTUP_TOOLCHAIN`.
    Environment,
    /// Nearest physical directory override in rustup settings.
    Directory,
    /// Nearest selected toolchain file.
    File,
    /// Rustup settings' default toolchain.
    Default,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: String,
    default_host_triple: Option<String>,
    default_toolchain: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    overrides: BTreeMap<String, String>,
    auto_self_update: Option<String>,
    auto_install: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainFile {
    toolchain: ToolchainSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainSection {
    channel: String,
    components: Option<Vec<String>>,
    targets: Option<Vec<String>>,
    profile: Option<String>,
}

fn physical_spelling(path: &Path) -> Result<(), Failure> {
    paths::validate(path)?;
    if !path.is_absolute()
        || path
            .to_str()
            .ok_or(Failure::Cwd)?
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(Failure::Cwd);
    }
    Ok(())
}

fn bounded_value(value: &Value, depth: usize) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Limit);
    }
    match value {
        Value::Table(table) => {
            if table.len() > limits::EXECUTION_ENVIRONMENT {
                return Err(Failure::Limit);
            }
            for (key, value) in table {
                if key.len() > limits::PATH_BYTES || key.contains('\0') {
                    return Err(Failure::Limit);
                }
                bounded_value(value, depth + 1)?;
            }
        }
        Value::Array(array) => {
            if array.len() > limits::EXECUTION_ENVIRONMENT {
                return Err(Failure::Limit);
            }
            for value in array {
                bounded_value(value, depth + 1)?;
            }
        }
        Value::String(value) if value.len() > limits::PATH_BYTES || value.contains('\0') => {
            return Err(Failure::Limit);
        }
        _ => {}
    }
    Ok(())
}

fn document(bytes: &[u8]) -> Result<Value, Failure> {
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::Toolchain)?;
    let value = toml::from_str(text).map_err(|_| Failure::Toolchain)?;
    bounded_value(&value, 0)?;
    Ok(value)
}

fn pin(value: &str, contract: &RustExecution) -> Result<(), Failure> {
    if value != contract.toolchain() && value != "1.94.0" {
        return Err(Failure::Toolchain);
    }
    Ok(())
}

fn file_pin(bytes: &[u8], legacy: bool, contract: &RustExecution) -> Result<(), Failure> {
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::Toolchain)?;
    // Rustup 1.28.2 treats a one-line extensionless file as a legacy name,
    // even if that line looks like TOML. Do not accept a different parse.
    if legacy && text.lines().count() == 1 {
        if !text.is_ascii() {
            return Err(Failure::Toolchain);
        }
        return pin(text.trim(), contract);
    }
    let file: ToolchainFile = document(bytes)?
        .try_into()
        .map_err(|_| Failure::Toolchain)?;
    pin(&file.toolchain.channel, contract)?;
    // Components/targets/profile can request installations even for an
    // installed release. No discovery or installation is performed here.
    if file
        .toolchain
        .components
        .is_some_and(|components| !components.is_empty())
        || file
            .toolchain
            .targets
            .is_some_and(|targets| !targets.is_empty())
        || file.toolchain.profile.is_some()
    {
        return Err(Failure::Toolchain);
    }
    Ok(())
}

fn settings<P: ReadProvider>(home: &Path, reads: &mut ReadSet<'_, P>) -> Result<Settings, Failure> {
    let bytes = reads
        .read(&home.join("settings.toml"))?
        .ok_or(Failure::Toolchain)?;
    let settings: Settings = document(bytes)?
        .try_into()
        .map_err(|_| Failure::Toolchain)?;
    if settings.version != "12"
        || settings
            .default_host_triple
            .as_deref()
            .is_some_and(|host| host != HOST)
        || settings
            .profile
            .as_deref()
            .is_some_and(|profile| !matches!(profile, "minimal" | "default" | "complete"))
        || settings.auto_install.as_deref() != Some("disable")
        || settings
            .auto_self_update
            .as_deref()
            .is_some_and(|mode| mode != "disable")
    {
        return Err(Failure::Toolchain);
    }
    for path in settings.overrides.keys() {
        physical_spelling(Path::new(path))?;
    }
    // Fallback settings introduce another unprotected selector source. This
    // first contract requires confirmed absence instead of silently ignoring it.
    if reads
        .read(Path::new("/etc/rustup/settings.toml"))?
        .is_some()
    {
        return Err(Failure::Toolchain);
    }
    Ok(settings)
}

/// Private bounded selector and installed-directory evidence. This alone is
/// not execution proof: call `verify_installed` and the contract's locality
/// recheck adjacent to completion, together with the shared read-set recheck.
pub struct Selection {
    directory: PathBuf,
    origin: Origin,
}

impl Selection {
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
    /// Resolve the pinned selector from proven physical execution cwd.
    /// Cargo child environment is checked separately from Cargo's own inputs.
    /// No manifest relocation, rustup discovery or tool execution takes place.
    ///
    /// # Errors
    /// Conflicting selectors, uninstalled trees, unsupported additions/settings,
    /// ambiguous path aliases, invalid inputs and shared budget exhaustion deny.
    pub fn resolve<P: ReadProvider>(
        cwd: &Path,
        invocation: &Invocation,
        child: &Environment,
        contract: &RustExecution,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        physical_spelling(cwd)?;
        let environment = invocation.environment();
        for key in ["HOME", "RUSTUP_HOME"] {
            if child.get(key) != environment.get(key) {
                return Err(Failure::Toolchain);
            }
        }
        if let Some(selector) = child.get("RUSTUP_TOOLCHAIN") {
            pin(selector, contract)?;
        }
        let home = environment.get("RUSTUP_HOME").map_or_else(
            || {
                let home = environment.get("HOME").ok_or(Failure::Environment)?;
                Ok(paths::absolute(cwd, Path::new(home))?.join(".rustup"))
            },
            |home| paths::absolute(cwd, Path::new(home)),
        )?;
        physical_spelling(&home)?;
        let settings = settings(&home, reads)?;
        let mut nearest = None;
        for directory in cwd.ancestors() {
            reads.checkpoint()?;
            let legacy = reads
                .read(&directory.join("rust-toolchain"))?
                .map(<[u8]>::to_vec);
            let extended = reads
                .read(&directory.join("rust-toolchain.toml"))?
                .map(<[u8]>::to_vec);
            if nearest.is_none() {
                if let Some(selector) = settings
                    .overrides
                    .get(directory.to_str().ok_or(Failure::Cwd)?)
                {
                    pin(selector, contract)?;
                    nearest = Some(Origin::Directory);
                } else if let Some(bytes) = legacy {
                    file_pin(&bytes, true, contract)?;
                    nearest = Some(Origin::File);
                } else if let Some(bytes) = extended {
                    file_pin(&bytes, false, contract)?;
                    nearest = Some(Origin::File);
                }
            }
        }
        // Agreement is deliberately stricter than rustup precedence: a nearest
        // directory/file conflict denies even when an explicit pin shadows it.
        let origin = if invocation.explicit_toolchain() {
            Origin::Command
        } else if let Some(selector) = environment.get("RUSTUP_TOOLCHAIN") {
            pin(selector, contract)?;
            Origin::Environment
        } else if let Some(origin) = nearest {
            origin
        } else {
            pin(
                settings
                    .default_toolchain
                    .as_deref()
                    .ok_or(Failure::Toolchain)?,
                contract,
            )?;
            Origin::Default
        };
        let directory = home.join("toolchains").join(contract.toolchain());
        physical_spelling(&directory)?;
        let entries = reads.directory(&directory)?;
        for required in ["bin", "lib"] {
            if !entries
                .iter()
                .any(|entry| entry.name() == required && entry.is_directory())
            {
                return Err(Failure::Toolchain);
            }
        }
        reads.checkpoint()?;
        Ok(Self { directory, origin })
    }

    /// Recheck all installed secondary executable identities without running them.
    /// A directory's existence alone is insufficient proof of an installed pin.
    ///
    /// # Errors
    /// Replacements, missing tools, untrusted aliases and late checks deny.
    pub fn verify_installed(
        &self,
        contract: &RustExecution,
        deadline: Instant,
    ) -> Result<(), Failure> {
        contract
            .verify_installed_toolchain(&self.directory, deadline)
            .map_err(|_| Failure::Executable)
    }

    /// Safe origin for compact provenance; no private selector value is returned.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::config::execution::literal_fixture;
    use crate::execution_input::ExecutionInput;
    use crate::rust_build::command;
    use crate::rust_build::environment::Origin as EnvironmentOrigin;
    use crate::rust_build::files::ReadSet;
    use crate::rust_build::files::fixtures::Scripted;

    use super::{Failure, Origin, Selection};

    fn fixture() -> Scripted {
        let provider = Scripted::default();
        provider.put("/sealed/rustup/settings.toml", "version='12'\nauto_install='disable'\ndefault_toolchain='1.94.0-x86_64-unknown-linux-gnu'\n");
        provider.list(
            "/sealed/rustup/toolchains/1.94.0-x86_64-unknown-linux-gnu",
            &[("bin", true), ("lib", true)],
        );
        provider
    }

    fn resolve(provider: &Scripted, source: &str) -> Result<Origin, Failure> {
        let (contract, _directory) = literal_fixture();
        let invocation = command::parse(
            &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
            &contract,
        )?;
        let mut reads = ReadSet::new(provider, Instant::now() + Duration::from_secs(5));
        let selection = Selection::resolve(
            Path::new("/work/nested"),
            &invocation,
            invocation.environment(),
            &contract,
            &mut reads,
        )?;
        reads.recheck()?;
        Ok(selection.origin())
    }

    #[test]
    fn selectors_follow_explicit_environment_directory_file_default_order() {
        let provider = fixture();
        assert_eq!(resolve(&provider, "cargo build"), Ok(Origin::Default));
        assert_eq!(
            resolve(&provider, "cargo +1.94.0 build"),
            Ok(Origin::Command)
        );
        assert_eq!(
            resolve(&provider, "rustup run 1.94.0 cargo build"),
            Ok(Origin::Command)
        );
        assert_eq!(
            resolve(&provider, "RUSTUP_TOOLCHAIN=1.94.0 cargo build"),
            Ok(Origin::Environment)
        );
        provider.put(
            "/work/rust-toolchain.toml",
            "[toolchain]\nchannel='1.94.0'\n",
        );
        assert_eq!(resolve(&provider, "cargo build"), Ok(Origin::File));
        provider.put(
            "/sealed/rustup/settings.toml",
            "version='12'\nauto_install='disable'\n[overrides]\n'/work'='1.94.0'\n",
        );
        assert_eq!(resolve(&provider, "cargo build"), Ok(Origin::Directory));
    }

    #[test]
    fn proximity_and_extensionless_preference_preserve_absence_evidence() {
        let provider = fixture();
        provider.put(
            "/sealed/rustup/settings.toml",
            "version='12'\nauto_install='disable'\n[overrides]\n'/work'='nightly'\n",
        );
        provider.put("/work/nested/rust-toolchain", "1.94.0\n");
        provider.put(
            "/work/nested/rust-toolchain.toml",
            "[toolchain]\nchannel='nightly'\n",
        );
        assert_eq!(resolve(&provider, "cargo build"), Ok(Origin::File));
        assert!(
            provider
                .calls
                .borrow()
                .contains(&Path::new("/work/nested/rust-toolchain.toml").to_owned())
        );
        provider.put("/work/nested/rust-toolchain", "nightly\n");
        assert_eq!(
            resolve(&provider, "cargo +1.94.0 build"),
            Err(Failure::Toolchain)
        );
    }

    #[test]
    fn additions_custom_paths_invalid_versions_and_fallback_settings_deny() {
        for contents in [
            "[toolchain]\nchannel='1.94.0'\ncomponents=['rustfmt']",
            "[toolchain]\nchannel='1.94.0'\ntargets=['wasm32-unknown-unknown']",
            "[toolchain]\nchannel='1.94.0'\nprofile='minimal'",
            "[toolchain]\npath='/secret-sentinel'",
            "[toolchain]\nchannel='stable'",
            "[toolchain]\nchannel='1.94.0'\nunknown='secret-sentinel'",
            "channel='1.94.0'",
            "",
        ] {
            let provider = fixture();
            provider.put("/work/nested/rust-toolchain.toml", contents);
            let failure = resolve(&provider, "cargo build").unwrap_err();
            assert!(!format!("{failure:?}").contains("secret-sentinel"));
        }
        for contents in [
            "version='2'\nauto_install='disable'",
            "version='12'\nauto_install='enable'",
            "version='12'",
            "version='12'\nauto_install='disable'\ndefault_host_triple='aarch64-unknown-linux-gnu'",
            "version='12'\nauto_install='disable'\nauto_self_update='enable'",
            "version='12'\nauto_install='disable'\n[overrides]\n'/work/../work'='1.94.0'",
        ] {
            let provider = fixture();
            provider.put("/sealed/rustup/settings.toml", contents);
            assert!(resolve(&provider, "cargo +1.94.0 build").is_err());
        }
        let provider = fixture();
        provider.put("/etc/rustup/settings.toml", "default_toolchain='nightly'");
        assert_eq!(resolve(&provider, "cargo build"), Err(Failure::Toolchain));
    }

    #[test]
    fn missing_installed_tree_and_child_environment_replacements_deny() {
        let provider = fixture();
        provider.directories.borrow_mut().clear();
        assert!(resolve(&provider, "cargo +1.94.0 build").is_err());
        let provider = fixture();
        let (contract, _directory) = literal_fixture();
        let invocation = command::parse(
            &ExecutionInput::from_value(&json!({"command":"cargo build"})).unwrap(),
            &contract,
        )
        .unwrap();
        for (key, value) in [
            ("RUSTUP_HOME", "/secret-sentinel"),
            ("HOME", "/secret-sentinel"),
            ("RUSTUP_TOOLCHAIN", "nightly"),
        ] {
            let mut child = invocation.environment().clone();
            child
                .set(key, Some(value), EnvironmentOrigin::CargoConfig)
                .unwrap();
            let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
            assert!(
                Selection::resolve(
                    Path::new("/work"),
                    &invocation,
                    &child,
                    &contract,
                    &mut reads
                )
                .is_err()
            );
        }
    }

    #[test]
    fn new_nearer_file_settings_and_installation_replacement_invalidate() {
        for path in [
            "/work/nested/rust-toolchain.toml",
            "/sealed/rustup/settings.toml",
        ] {
            let provider = fixture();
            let (contract, _directory) = literal_fixture();
            let invocation = command::parse(
                &ExecutionInput::from_value(&json!({"command":"cargo build"})).unwrap(),
                &contract,
            )
            .unwrap();
            let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
            Selection::resolve(
                Path::new("/work/nested"),
                &invocation,
                invocation.environment(),
                &contract,
                &mut reads,
            )
            .unwrap();
            provider.put(path, "secret-sentinel");
            assert_eq!(reads.recheck(), Err(Failure::Changed));
        }
        let provider = fixture();
        let (contract, _directory) = literal_fixture();
        let invocation = command::parse(
            &ExecutionInput::from_value(&json!({"command":"cargo build"})).unwrap(),
            &contract,
        )
        .unwrap();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
        let selection = Selection::resolve(
            Path::new("/work/nested"),
            &invocation,
            invocation.environment(),
            &contract,
            &mut reads,
        )
        .unwrap();
        assert_eq!(
            selection.verify_installed(&contract, Instant::now() + Duration::from_secs(5)),
            Err(Failure::Executable)
        );
        provider.list(
            "/sealed/rustup/toolchains/1.94.0-x86_64-unknown-linux-gnu",
            &[("bin", true)],
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
    }

    #[test]
    fn shared_bounds_deadline_and_nonexecution_apply_to_selector_inputs() {
        let provider = fixture();
        let (contract, directory) = literal_fixture();
        let invocation = command::parse(
            &ExecutionInput::from_value(&json!({"command":"cargo build"})).unwrap(),
            &contract,
        )
        .unwrap();
        let mut reads = ReadSet::new(
            &provider,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        );
        assert!(
            Selection::resolve(
                Path::new("/work"),
                &invocation,
                invocation.environment(),
                &contract,
                &mut reads
            )
            .is_err()
        );
        let deep = format!("/{}", "nested/".repeat(40));
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
        assert_eq!(
            Selection::resolve(
                Path::new(&deep),
                &invocation,
                invocation.environment(),
                &contract,
                &mut reads
            )
            .err(),
            Some(Failure::Limit)
        );
        for name in ["cargo", "rustc", "rustdoc", "rustup"] {
            assert_eq!(
                std::fs::read(directory.path().join(name)).unwrap(),
                b"NONEXECUTION_SENTINEL"
            );
        }
        assert!(!directory.path().join("marker").exists());
    }
}
