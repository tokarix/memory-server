//! Cargo 1.94 literal selection and directory layout, without running Cargo.
//!
//! A layout is a required directory inventory, not a complete build proof.
//! Existing descendants, execution identity, input rechecks and storage backing
//! must still be checked by its consumer. No profile field grants policy authority.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use toml::Value;

use crate::config::execution::RustExecution;
use crate::limits;
use crate::storage::{Candidate, CandidateRole, MAX_CANDIDATES};

use super::command::{Invocation, cargo_action};
use super::config::Configuration;
use super::environment::Environment;
use super::files::MAX_DEPTH;
use super::manifest::{Selection, Sources};
use super::{Action, BuildAction, Failure, paths};

/// Private literal selections from one already-classified Cargo invocation.
/// This structure deliberately has no Debug or Serialize implementation.
pub struct Options {
    action: BuildAction,
    selection: Selection,
    configuration: Vec<String>,
    target_directory: Option<String>,
    targets: Vec<String>,
    profile: String,
    install_path: Option<PathBuf>,
    install_root: Option<String>,
    timings: bool,
}

fn unique(value: &mut Option<String>, next: &str) -> Result<(), Failure> {
    if value.replace(next.to_owned()).is_some() {
        return Err(Failure::Unsupported);
    }
    Ok(())
}

fn valued(flag: &str) -> bool {
    matches!(
        flag,
        "--config"
            | "--color"
            | "--target-dir"
            | "--target"
            | "--profile"
            | "--features"
            | "-F"
            | "--jobs"
            | "-j"
            | "--message-format"
            | "--bin"
            | "--example"
            | "--manifest-path"
            | "--package"
            | "-p"
            | "--exclude"
            | "--test"
            | "--bench"
            | "--path"
            | "--root"
            | "--git"
            | "--branch"
            | "--tag"
            | "--rev"
            | "--registry"
            | "--version"
    )
}

impl Options {
    /// Extract only the grammar's supported options, consuming values and the
    /// command name exactly once. Runtime run/test/bench arguments stay opaque
    /// execution data after `--` and cannot change Cargo's static layout.
    ///
    /// # Errors
    /// Non-Cargo actions, ambiguous selectors, remote install and overflow deny.
    pub fn extract(invocation: &Invocation, cwd: &Path) -> Result<Self, Failure> {
        let Action::Build(action) = invocation.action() else {
            return Err(Failure::Unsupported);
        };
        if invocation.tool() != "cargo" {
            return Err(Failure::Unsupported);
        }
        let mut options = Self {
            action,
            selection: Selection::default(),
            configuration: Vec::new(),
            target_directory: None,
            targets: Vec::new(),
            profile: String::new(),
            install_path: None,
            install_root: None,
            timings: false,
        };
        let mut profile = None;
        let mut preset = None;
        let mut source = None;
        let mut manifest = None;
        let mut command = false;
        let mut position = 0;
        let words = invocation.arguments();
        while let Some(word) = words.get(position) {
            let (flag, inline) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(flag, value)| (flag, Some(value)));
            if valued(flag) {
                let value = if let Some(value) = inline {
                    value
                } else {
                    position += 1;
                    words.get(position).ok_or(Failure::Syntax)?
                };
                options.value(flag, value, &mut profile, &mut source, &mut manifest)?;
            } else if !command && cargo_action(word) == Some(action) {
                command = true;
            } else if word == "--" && command {
                break;
            } else {
                match word.as_str() {
                    "--release" => unique(&mut preset, "release")?,
                    "--debug" if action == BuildAction::Install => unique(&mut preset, "dev")?,
                    "--workspace" | "--all" => options.selection.workspace = true,
                    "--timings" => options.timings = true,
                    value if !value.starts_with('-') && action == BuildAction::Install => {
                        options.selection.packages.push(value.to_owned());
                    }
                    _ => {}
                }
            }
            position += 1;
        }
        if !command || (preset.is_some() && profile.is_some()) {
            return Err(Failure::Unsupported);
        }
        options.profile = profile.or(preset).unwrap_or_else(|| {
            match action {
                BuildAction::Bench => "bench",
                BuildAction::Test => "test",
                BuildAction::Install => "release",
                _ => "dev",
            }
            .to_owned()
        });
        profile_name(&options.profile)?;
        if action == BuildAction::Install {
            let path =
                paths::absolute(cwd, Path::new(source.as_deref().ok_or(Failure::Manifest)?))?;
            options.selection.manifest = Some(path.join("Cargo.toml"));
            options.install_path = Some(path);
        } else {
            options.selection.manifest = manifest.map(PathBuf::from);
        }
        Ok(options)
    }

    fn value(
        &mut self,
        flag: &str,
        value: &str,
        profile: &mut Option<String>,
        source: &mut Option<String>,
        manifest: &mut Option<String>,
    ) -> Result<(), Failure> {
        paths::validate(Path::new(value))?;
        match flag {
            "--config" => self.configuration.push(value.to_owned()),
            "--target-dir" => unique(&mut self.target_directory, value)?,
            "--target" => self.targets.push(value.to_owned()),
            "--profile" => unique(profile, value)?,
            "--manifest-path" => unique(manifest, value)?,
            "--package" | "-p" => self.selection.packages.push(value.to_owned()),
            "--exclude" => self.selection.exclude.push(value.to_owned()),
            "--path" => unique(source, value)?,
            "--root" => unique(&mut self.install_root, value)?,
            "--git" | "--branch" | "--tag" | "--rev" | "--registry" | "--version" => {
                return Err(Failure::Manifest);
            }
            _ => {}
        }
        Ok(())
    }

    /// Manifest/workspace/package selection, separate from configuration cwd.
    #[must_use]
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }
    /// Repeated --config values in their original merge order.
    #[must_use]
    pub fn configuration(&self) -> &[String] {
        &self.configuration
    }
    /// Normal operations discover at cwd; local install discovers at --path.
    #[must_use]
    pub fn discovery_root<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        self.install_path.as_deref().unwrap_or(cwd)
    }
}

fn profile_name(name: &str) -> Result<(), Failure> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || matches!(
            name,
            "debug"
                | "build"
                | "deps"
                | "examples"
                | "incremental"
                | "package"
                | "doc"
                | "cargo-timings"
        )
    {
        return Err(Failure::Configuration);
    }
    Ok(())
}

fn profile_fields(value: &Value, override_kind: u8, depth: usize) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Limit);
    }
    for (key, value) in value.as_table().ok_or(Failure::Configuration)? {
        let valid = match key.as_str() {
            "inherits" if override_kind == 0 => {
                profile_name(value.as_str().ok_or(Failure::Configuration)?)?;
                true
            }
            "opt-level" => {
                value
                    .as_integer()
                    .is_some_and(|value| (0..=3).contains(&value))
                    || value
                        .as_str()
                        .is_some_and(|value| matches!(value, "s" | "z"))
            }
            "debug" => {
                value.is_bool()
                    || value
                        .as_integer()
                        .is_some_and(|value| (0..=2).contains(&value))
                    || value.as_str().is_some_and(|value| {
                        matches!(
                            value,
                            "none"
                                | "limited"
                                | "full"
                                | "line-tables-only"
                                | "line-directives-only"
                        )
                    })
            }
            "debug-assertions" | "overflow-checks" | "incremental" => value.is_bool(),
            "rpath" if override_kind != 2 => value.is_bool(),
            "codegen-units" => value
                .as_integer()
                .is_some_and(|value| (1..=256).contains(&value)),
            "split-debuginfo" => value
                .as_str()
                .is_some_and(|value| matches!(value, "off" | "packed" | "unpacked")),
            "strip" => {
                value.is_bool()
                    || value
                        .as_str()
                        .is_some_and(|value| matches!(value, "none" | "debuginfo" | "symbols"))
            }
            "panic" if override_kind != 2 => value
                .as_str()
                .is_some_and(|value| matches!(value, "abort" | "unwind")),
            "lto" if override_kind != 2 => {
                value.is_bool()
                    || value
                        .as_str()
                        .is_some_and(|value| matches!(value, "off" | "thin" | "fat"))
            }
            "build-override" if override_kind == 0 => {
                profile_fields(value, 1, depth + 1)?;
                true
            }
            "package" if override_kind == 0 => {
                for (name, value) in value.as_table().ok_or(Failure::Configuration)? {
                    if name.len() > limits::PATH_BYTES || name.contains('\0') {
                        return Err(Failure::Limit);
                    }
                    profile_fields(value, 2, depth + 1)?;
                }
                true
            }
            _ => false,
        };
        if !valid {
            return Err(Failure::Configuration);
        }
    }
    Ok(())
}

pub(super) fn validate_profiles(value: &Value) -> Result<(), Failure> {
    let profiles = value.as_table().ok_or(Failure::Configuration)?;
    if profiles.len() > 64 {
        return Err(Failure::Limit);
    }
    for (name, value) in profiles {
        profile_name(name)?;
        profile_fields(value, 0, 0)?;
    }
    Ok(())
}

fn merge(lower: &mut Value, higher: &Value) -> Result<(), Failure> {
    let lower = lower.as_table_mut().ok_or(Failure::Configuration)?;
    for (key, value) in higher.as_table().ok_or(Failure::Configuration)? {
        if value.is_table() && lower.get(key).is_some_and(Value::is_table) {
            merge(lower.get_mut(key).ok_or(Failure::Configuration)?, value)?;
        } else {
            lower.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn profile_directory(
    name: &str,
    manifest: &Value,
    configuration: Option<&Value>,
) -> Result<String, Failure> {
    let mut profiles = manifest
        .get("profile")
        .cloned()
        .unwrap_or_else(|| Value::Table(toml::Table::new()));
    validate_profiles(&profiles)?;
    if let Some(configuration) = configuration {
        validate_profiles(configuration)?;
        merge(&mut profiles, configuration)?;
    }
    validate_profiles(&profiles)?;
    for builtin in ["test", "bench"] {
        let table = profiles.as_table_mut().ok_or(Failure::Configuration)?;
        let value = table
            .entry(builtin.to_owned())
            .or_insert_with(|| Value::Table(toml::Table::new()));
        value
            .as_table_mut()
            .ok_or(Failure::Configuration)?
            .entry("inherits".to_owned())
            .or_insert_with(|| {
                Value::String(if builtin == "test" { "dev" } else { "release" }.to_owned())
            });
    }
    for root in ["dev", "release"] {
        if profiles
            .get(root)
            .and_then(|value| value.get("inherits"))
            .is_some()
        {
            return Err(Failure::Configuration);
        }
    }
    // Cargo validates all manifest profile inheritance, not just the selected one.
    for start in profiles
        .as_table()
        .ok_or(Failure::Configuration)?
        .keys()
        .chain(std::iter::once(&name.to_owned()))
    {
        let mut current = start.as_str();
        let mut seen = BTreeSet::new();
        while !matches!(current, "dev" | "release") {
            if !seen.insert(current) {
                return Err(Failure::Configuration);
            }
            if seen.len() > MAX_DEPTH {
                return Err(Failure::Limit);
            }
            current = profiles
                .get(current)
                .and_then(|value| value.get("inherits"))
                .and_then(Value::as_str)
                .ok_or(Failure::Configuration)?;
        }
    }
    Ok(match name {
        "dev" | "test" => "debug",
        "bench" => "release",
        value => value,
    }
    .to_owned())
}

fn target_name(value: &str, host: &str) -> Result<String, Failure> {
    let value = if value == "host-tuple" { host } else { value };
    // An explicit small built-in list excludes JSON targets, RUST_TARGET_PATH
    // lookups, future target semantics and opaque target/linker configuration.
    if ![
        host,
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "wasm32-unknown-unknown",
    ]
    .contains(&value)
    {
        return Err(Failure::Unsupported);
    }
    Ok(value.to_owned())
}

/// One required directory with a distinct role, even if paths coincide.
pub struct Directory {
    role: CandidateRole,
    path: PathBuf,
}

impl Directory {
    /// Private absolute directory spelling preserving physical component semantics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Explicit storage role.
    #[must_use]
    pub const fn role(&self) -> CandidateRole {
        self.role
    }
}

/// Required stable layout roots. This alone does not cover existing descendants.
/// Consumers must not use this as a `CompleteBuild` or authorize execution from it.
pub struct Layout {
    directories: Vec<Directory>,
    units: Vec<UnitLayout>,
}

/// One host or explicitly selected target's profile layout, without unit hashes.
pub struct UnitLayout {
    artifact: PathBuf,
    intermediate: PathBuf,
    temporary: PathBuf,
    target: Option<String>,
}

impl UnitLayout {
    /// Final profile directory for executable artifacts.
    #[must_use]
    pub fn artifact(&self) -> &Path {
        &self.artifact
    }
    /// Intermediate profile directory containing deps and incremental outputs.
    #[must_use]
    pub fn intermediate(&self) -> &Path {
        &self.intermediate
    }
    /// Cargo's generated integration-test/bench temporary directory.
    #[must_use]
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }
    /// Explicit target; absence denotes host units.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }
}

impl Layout {
    /// Resolve all static layout roots for a supported local Cargo operation.
    /// `compiler` must be the execution model's effective compiler environment,
    /// after Rustup/Clippy generation and Cargo child [env].
    ///
    /// # Errors
    /// Unsupported profiles/targets, missing source selection and overflow deny.
    pub fn resolve(
        options: &Options,
        configuration: &Configuration,
        sources: &Sources,
        contract: &RustExecution,
        environments: (&Environment, &Environment),
        cwd: &Path,
    ) -> Result<Self, Failure> {
        let (process, compiler) = environments;
        let mut layout = Self {
            directories: Vec::new(),
            units: Vec::new(),
        };
        for source in sources.directories() {
            layout.push(CandidateRole::SourceWorktree, source.to_owned())?;
        }
        let target = configuration.target_directory(
            sources.workspace(),
            options.target_directory.as_deref(),
            process,
        )?;
        let build = configuration.build_directory(target.path(), process)?;
        layout.push(CandidateRole::TargetDirectory, target.path().to_owned())?;
        layout.push(CandidateRole::BuildOutput, build.path().to_owned())?;
        layout.push(
            CandidateRole::CompilerTemporary,
            paths::absolute(cwd, Path::new(compiler.get("TMPDIR").unwrap_or("/tmp")))?,
        )?;
        // Cargo may update cache tracking/locks and clean any existing cache,
        // including in a local path-only build with no downloadable dependencies.
        let home = configuration.cargo_home();
        layout.push(CandidateRole::BuildOutput, home.to_owned())?;
        for suffix in [
            "registry",
            "registry/index",
            "registry/cache",
            "registry/src",
            "git",
            "git/db",
            "git/checkouts",
        ] {
            layout.push(CandidateRole::BuildOutput, home.join(suffix))?;
        }
        let profile = profile_directory(
            &options.profile,
            sources.workspace_manifest(),
            configuration.profiles()?.as_ref(),
        )?;
        let host = contract
            .toolchain()
            .strip_prefix("1.94.0-")
            .ok_or(Failure::Toolchain)?;
        let targets = configuration.targets(
            options.action == BuildAction::Install,
            &options.targets,
            process,
        )?;
        let mut names = BTreeSet::new();
        for target in targets {
            names.insert(target_name(&target, host)?);
        }
        // Host units exist even when every selected package is cross-targeted.
        layout.outputs(target.path(), build.path(), &profile, options)?;
        for name in names {
            layout.outputs(
                &target.path().join(&name),
                &build.path().join(&name),
                &profile,
                options,
            )?;
            layout.units.last_mut().ok_or(Failure::Unsupported)?.target = Some(name);
        }
        if options.action == BuildAction::Install {
            let root = configuration.install_root(options.install_root.as_deref(), process)?;
            layout.push(CandidateRole::BuildOutput, root.path().to_owned())?;
            layout.push(CandidateRole::BuildOutput, root.path().join("bin"))?;
        }
        Ok(layout)
    }

    fn outputs(
        &mut self,
        target: &Path,
        build: &Path,
        profile: &str,
        options: &Options,
    ) -> Result<(), Failure> {
        self.push(CandidateRole::BuildOutput, target.to_owned())?;
        self.push(CandidateRole::BuildOutput, build.to_owned())?;
        let artifacts = target.join(profile);
        let intermediate = build.join(profile);
        self.units.push(UnitLayout {
            artifact: artifacts.clone(),
            intermediate: intermediate.clone(),
            temporary: build.join("tmp"),
            target: None,
        });
        self.push(CandidateRole::BuildOutput, artifacts.clone())?;
        self.push(CandidateRole::BuildOutput, artifacts.join("examples"))?;
        self.push(CandidateRole::BuildOutput, intermediate.clone())?;
        for suffix in [
            "deps",
            "deps/artifact",
            "examples",
            "incremental",
            ".fingerprint",
            "build",
        ] {
            self.push(CandidateRole::BuildOutput, intermediate.join(suffix))?;
        }
        self.push(CandidateRole::BuildOutput, build.join("tmp"))?;
        if options.action == BuildAction::Doc {
            self.push(CandidateRole::BuildOutput, target.join("doc"))?;
        }
        if options.timings {
            self.push(CandidateRole::BuildOutput, target.join("cargo-timings"))?;
        }
        Ok(())
    }

    /// Stable host-first layout evidence for generated child environments.
    #[must_use]
    pub fn units(&self) -> &[UnitLayout] {
        &self.units
    }

    fn push(&mut self, role: CandidateRole, path: PathBuf) -> Result<(), Failure> {
        paths::validate(&path)?;
        if !path.is_absolute() {
            return Err(Failure::Cwd);
        }
        if self
            .directories
            .iter()
            .any(|item| item.role == role && item.path == path)
        {
            return Ok(());
        }
        if self.directories.len() >= MAX_CANDIDATES {
            return Err(Failure::Limit);
        }
        self.directories.push(Directory { role, path });
        Ok(())
    }

    /// Stable required roots; duplicate paths retain different role coverage.
    #[must_use]
    pub fn directories(&self) -> &[Directory] {
        &self.directories
    }

    /// Explicit directory candidates for later backing assessment. Calling this
    /// does not establish descendant coverage or execution locality.
    ///
    /// # Errors
    /// The storage evaluator's fixed path/index bounds remain authoritative.
    pub fn candidates(&self) -> Result<Vec<Candidate>, Failure> {
        self.directories
            .iter()
            .enumerate()
            .map(|(index, item)| {
                Candidate::new(
                    u8::try_from(index).map_err(|_| Failure::Limit)?,
                    item.role,
                    &item.path,
                )
                .map_err(|_| Failure::Limit)
            })
            .collect()
    }

    /// Domain-separated private operation-layout digest; no path serialization.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-cargo-layout-v1\0");
        for item in &self.directories {
            hash.update([match item.role {
                CandidateRole::TargetDirectory => 0,
                CandidateRole::BuildOutput => 1,
                CandidateRole::CompilerTemporary => 2,
                CandidateRole::SourceWorktree => 3,
            }]);
            let bytes = item.path.as_os_str().as_encoded_bytes();
            hash.update(bytes.len().to_be_bytes());
            hash.update(bytes);
        }
        hash.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{Layout, Options, profile_directory};
    use crate::config::execution::literal_fixture;
    use crate::execution_input::ExecutionInput;
    use crate::rust_build::Failure;
    use crate::rust_build::command::parse;
    use crate::rust_build::config::{Configuration, Origin};
    use crate::rust_build::environment::Environment;
    use crate::rust_build::files::{ReadSet, fixtures::Scripted};
    use crate::rust_build::manifest::Sources;
    use crate::storage::CandidateRole;

    fn input(source: &str) -> ExecutionInput {
        ExecutionInput::from_value(&json!({"command":source})).unwrap()
    }

    fn fixture() -> Scripted {
        let provider = Scripted::default();
        provider.put(
            "/work/Cargo.toml",
            "[package]\nname='literal'\nversion='1.0.0'\n",
        );
        provider
    }

    fn resolve(source: &str, provider: &Scripted) -> Result<Layout, Failure> {
        let (contract, _files) = literal_fixture();
        let cwd = Path::new("/work");
        let invocation = parse(&input(source), &contract)?;
        let options = Options::extract(&invocation, cwd)?;
        let mut reads = ReadSet::new(provider, Instant::now() + Duration::from_secs(2));
        let configuration = Configuration::load(
            cwd,
            Some(options.discovery_root(cwd)),
            invocation.environment(),
            options.configuration(),
            &mut reads,
        )?;
        let sources = Sources::resolve(cwd, options.selection(), &mut reads)?;
        let compiler = configuration.child_environment(invocation.environment())?;
        let layout = Layout::resolve(
            &options,
            &configuration,
            &sources,
            &contract,
            (invocation.environment(), &compiler),
            cwd,
        )?;
        reads.recheck()?;
        Ok(layout)
    }

    fn has(layout: &Layout, role: CandidateRole, path: &str) -> bool {
        layout
            .directories()
            .iter()
            .any(|item| item.role() == role && item.path() == Path::new(path))
    }

    #[test]
    fn every_named_cargo_action_has_separate_required_static_roles() {
        let provider = fixture();
        for (source, profile) in [
            ("cargo build", "debug"),
            ("cargo b", "debug"),
            ("cargo check", "debug"),
            ("cargo c", "debug"),
            ("cargo test --no-run", "debug"),
            ("cargo t -- --list", "debug"),
            ("cargo clippy", "debug"),
            ("cargo run -- --profile secret-sentinel", "debug"),
            ("cargo r", "debug"),
            ("cargo bench --no-run", "release"),
            ("cargo bench -- --list", "release"),
            ("cargo doc", "debug"),
            ("cargo d", "debug"),
            ("cargo install --path .", "release"),
        ] {
            let layout = resolve(source, &provider).unwrap();
            assert!(has(&layout, CandidateRole::SourceWorktree, "/work"));
            assert!(has(&layout, CandidateRole::TargetDirectory, "/work/target"));
            assert!(has(
                &layout,
                CandidateRole::CompilerTemporary,
                "/sealed/tmp"
            ));
            for suffix in [
                "deps",
                "deps/artifact",
                "examples",
                "incremental",
                ".fingerprint",
                "build",
            ] {
                assert!(has(
                    &layout,
                    CandidateRole::BuildOutput,
                    &format!("/work/target/{profile}/{suffix}")
                ));
            }
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                "/sealed/cargo-home/registry/src"
            ));
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                "/sealed/cargo-home/git/checkouts"
            ));
            let candidates = layout.candidates().unwrap();
            assert!(
                candidates
                    .iter()
                    .enumerate()
                    .all(|(index, item)| usize::from(item.index()) == index)
            );
        }
    }

    #[test]
    fn target_build_artifact_doc_and_timing_paths_do_not_collapse() {
        let provider = fixture();
        provider.put("/work/.cargo/config.toml", "[build]\ntarget-dir='final'\nbuild-dir='intermediate'\ntarget='aarch64-unknown-linux-gnu'\n");
        let layout = resolve("cargo doc --timings", &provider).unwrap();
        for prefix in [
            "/work/intermediate/debug",
            "/work/intermediate/aarch64-unknown-linux-gnu/debug",
        ] {
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                &format!("{prefix}/deps")
            ));
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                &format!("{prefix}/build")
            ));
        }
        for prefix in ["/work/final", "/work/final/aarch64-unknown-linux-gnu"] {
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                &format!("{prefix}/doc")
            ));
            assert!(has(
                &layout,
                CandidateRole::BuildOutput,
                &format!("{prefix}/cargo-timings")
            ));
        }
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/intermediate/tmp"
        ));
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/intermediate/aarch64-unknown-linux-gnu/tmp"
        ));
    }

    #[test]
    fn literal_options_have_ordered_config_values_and_separate_discovery() {
        let provider = fixture();
        provider.put(
            "/else/Cargo.toml",
            "[package]\nname='elsewhere'\nversion='1.0.0'",
        );
        provider.put("/else/.cargo/config.toml", "[build]\ntarget-dir='ignored'");
        provider.put("/work/.cargo/config.toml", "[build]\ntarget-dir='from-cwd'");
        let layout = resolve("cargo --config 'build.target-dir=\"first\"' build --manifest-path /else/Cargo.toml --config 'build.target-dir=\"second\"'", &provider).unwrap();
        assert!(has(&layout, CandidateRole::TargetDirectory, "/work/second"));
        assert!(
            !provider
                .calls
                .borrow()
                .iter()
                .any(|path| path == Path::new("/else/.cargo/config.toml"))
        );
        let install = resolve(
            "cargo install --path /else --root '/work/install root' --debug",
            &provider,
        )
        .unwrap();
        assert!(has(
            &install,
            CandidateRole::TargetDirectory,
            "/else/ignored"
        ));
        assert!(has(
            &install,
            CandidateRole::BuildOutput,
            "/work/install root/bin"
        ));
        assert!(has(
            &install,
            CandidateRole::BuildOutput,
            "/else/ignored/debug/deps"
        ));
    }

    #[test]
    fn precedence_flag_special_environment_cli_configuration_and_child_environment() {
        let provider = fixture();
        provider.put("/work/.cargo/config.toml", "[build]\ntarget-dir='configured'\nbuild-dir='configured-build'\n[env]\nTMPDIR={value='child tmp',force=true,relative=true}\nCARGO_TARGET_DIR={value='child-only',force=true}");
        let source = "CARGO_TARGET_DIR=special CARGO_BUILD_TARGET_DIR=general CARGO_BUILD_BUILD_DIR=env-build cargo build --config 'build.target-dir=\"cli\"' --config 'build.build-dir=\"cli-build\"'";
        let layout = resolve(source, &provider).unwrap();
        assert!(has(
            &layout,
            CandidateRole::TargetDirectory,
            "/work/special"
        ));
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/cli-build/debug/deps"
        ));
        assert!(has(
            &layout,
            CandidateRole::CompilerTemporary,
            "/work/child tmp"
        ));
        let flag = resolve(
            &format!("{source} --target-dir 'literal target'"),
            &provider,
        )
        .unwrap();
        assert!(has(
            &flag,
            CandidateRole::TargetDirectory,
            "/work/literal target"
        ));
        assert!(!has(
            &flag,
            CandidateRole::TargetDirectory,
            "/work/child-only"
        ));
        let default_tmp = resolve("env -u TMPDIR cargo build", &fixture()).unwrap();
        assert!(has(&default_tmp, CandidateRole::CompilerTemporary, "/tmp"));
        assert!(resolve("TMPDIR='' cargo build", &fixture()).is_err());
    }

    #[test]
    fn config_target_precedence_multiple_targets_and_install_exclusion() {
        let provider = fixture();
        provider.put(
            "/work/.cargo/config.toml",
            "[build]\ntarget=['aarch64-unknown-linux-gnu']",
        );
        let layout = resolve(
            "cargo build --target wasm32-unknown-unknown --target host-tuple",
            &provider,
        )
        .unwrap();
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/target/wasm32-unknown-unknown/debug/deps"
        ));
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/target/x86_64-unknown-linux-gnu/debug/deps"
        ));
        assert!(!has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/target/aarch64-unknown-linux-gnu/debug/deps"
        ));
        let install = resolve("cargo install --path .", &provider).unwrap();
        assert!(!has(
            &install,
            CandidateRole::BuildOutput,
            "/work/target/aarch64-unknown-linux-gnu/release/deps"
        ));
        assert!(
            resolve(
                "CARGO_BUILD_TARGET=wasm32-unknown-unknown cargo build",
                &provider
            )
            .is_err()
        );
        let cli = resolve("CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu cargo build --config 'build.target=\"wasm32-unknown-unknown\"'", &fixture()).unwrap();
        assert!(has(
            &cli,
            CandidateRole::BuildOutput,
            "/work/target/wasm32-unknown-unknown/debug/deps"
        ));
    }

    #[test]
    fn custom_profiles_merge_fields_validate_inheritance_and_preserve_names() {
        let provider = fixture();
        provider.put("/work/Cargo.toml", "[package]\nname='literal'\nversion='1.0.0'\n[profile.quick]\ninherits='release'\nopt-level=1\n[profile.quick.package.literal]\ndebug=true");
        provider.put(
            "/work/.cargo/config.toml",
            "[profile.quick]\ninherits='dev'\n[profile.quick.build-override]\nopt-level=0",
        );
        let layout = resolve("cargo build --profile quick", &provider).unwrap();
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/target/quick/deps"
        ));
        let manifest: toml::Value =
            toml::from_str("[profile.a]\ninherits='b'\n[profile.b]\ninherits='a'").unwrap();
        assert_eq!(
            profile_directory("dev", &manifest, None),
            Err(Failure::Configuration)
        );
        for fields in [
            "inherits='missing'",
            "dir-name='secret-sentinel'",
            "rustflags=['-o','secret-sentinel']",
            "codegen-backend='secret-sentinel'",
            "package.literal.panic='abort'",
        ] {
            provider.put(
                "/work/.cargo/config.toml",
                &format!("[profile.quick]\n{fields}"),
            );
            assert!(resolve("cargo build --profile quick", &provider).is_err());
        }
    }

    #[test]
    fn coincident_paths_keep_source_target_temp_and_output_roles() {
        let layout = resolve(
            "TMPDIR=/work cargo build --target-dir /work --config 'build.build-dir=\"/work\"'",
            &fixture(),
        )
        .unwrap();
        for role in [
            CandidateRole::SourceWorktree,
            CandidateRole::TargetDirectory,
            CandidateRole::BuildOutput,
            CandidateRole::CompilerTemporary,
        ] {
            assert!(has(&layout, role, "/work"));
        }
        let second = resolve(
            "TMPDIR=/work cargo build --target-dir /work --config 'build.build-dir=\"/work\"'",
            &fixture(),
        )
        .unwrap();
        assert_eq!(layout.digest(), second.digest());
        assert_ne!(
            layout.digest(),
            resolve("cargo build", &fixture()).unwrap().digest()
        );
    }

    #[test]
    fn ambiguous_duplicate_future_and_remote_forms_never_claim_a_layout() {
        for source in [
            "cargo build --profile dev --release",
            "cargo build --profile dev --profile release",
            "cargo build --target-dir a --target-dir b",
            "cargo build --manifest-path a --manifest-path b",
            "cargo build --target custom.json",
            "cargo build --target secret-sentinel",
            "cargo build --profile secret-sentinel",
            "cargo install secret-sentinel",
            "cargo install --git https://secret-sentinel.invalid",
            "cargo install --path . --registry secret-sentinel",
            "cargo install --path . --path /else",
        ] {
            assert!(resolve(source, &fixture()).is_err(), "{source}");
        }
        let (contract, _files) = literal_fixture();
        assert!(
            Options::extract(
                &parse(&input("cargo --version"), &contract).unwrap(),
                Path::new("/work")
            )
            .is_err()
        );
    }

    #[test]
    fn layout_bounds_paths_and_config_selection_remain_read_only() {
        let provider = fixture();
        provider.put(
            "/work/.cargo/config.toml",
            "[build]\nbuild-dir='base/../intermediate'",
        );
        let layout = resolve("cargo build", &provider).unwrap();
        assert!(has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/base/../intermediate/debug/deps"
        ));
        assert!(!has(
            &layout,
            CandidateRole::BuildOutput,
            "/work/intermediate/debug/deps"
        ));
        let many = " --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu --target wasm32-unknown-unknown";
        provider.put("/work/.cargo/config.toml", "[build]\nbuild-dir='separate'");
        assert_eq!(
            resolve(&format!("cargo doc --timings{many}"), &provider).err(),
            Some(Failure::Limit)
        );
        let environment =
            Environment::inherited(&literal_fixture().0.environment().clone()).unwrap();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(2));
        let configuration = Configuration::load(
            Path::new("/work"),
            Some(Path::new("/work")),
            &environment,
            &[],
            &mut reads,
        )
        .unwrap();
        assert_eq!(
            configuration
                .target_directory(Path::new("/work"), None, &environment)
                .unwrap()
                .origin(),
            Origin::Default
        );
        provider.put("/work/.cargo/config", "[build]\ntarget-dir='replacement'");
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        assert!(
            provider
                .calls
                .borrow()
                .iter()
                .all(|path| path.file_name().is_some())
        );
    }
}
