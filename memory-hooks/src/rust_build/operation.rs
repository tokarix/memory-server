//! Complete static operations over the bounded, non-executing Rust contract.
//!
//! Completeness covers configured build destinations and modeled process
//! inputs. It supplies no locality, policy, claim or execution authorization.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};
use toml::Value;

use crate::config::execution::RustExecution;
use crate::storage::{Candidate, CandidateRole};

use super::cargo::{Layout, Options};
use super::command::Invocation;
use super::config::Configuration;
use super::execution::{Model, rustup_environment};
use super::files::{MAX_DEPTH, ReadProvider, ReadSet};
use super::inventory::Inventory;
use super::manifest::Sources;
use super::toolchain::Selection;
use super::{Action, BuildAction, Failure, paths};

/// An analyzed operation with retained inputs and complete static coverage.
/// Keep this private object alive until the final adjacent recheck. Raw
/// commands, environments and paths have no Debug or Serialize implementation.
pub struct Operation<'a, P: ReadProvider> {
    action: Action,
    model: Model,
    inventory: Option<Inventory>,
    reads: ReadSet<'a, P>,
    generated_digest: [u8; 32],
    deadline: Instant,
}

fn linker<P: ReadProvider>(
    flags: &[String],
    contract: &RustExecution,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    let mut selected = None;
    let mut flavor = None;
    let mut words = flags.iter();
    while let Some(word) = words.next() {
        if word != "-C" {
            continue;
        }
        let value = words.next().ok_or(Failure::Unsupported)?;
        if let Some(value) = value.strip_prefix("linker=") {
            if selected.replace(value).is_some() {
                return Err(Failure::Executable);
            }
        } else if let Some(value) = value.strip_prefix("linker-flavor=")
            && flavor.replace(value).is_some()
        {
            return Err(Failure::Executable);
        }
    }
    let pin = contract.native_linker().ok_or(Failure::Executable)?;
    if selected.map(Path::new) != Some(pin)
        || !matches!(flavor, Some("ld.lld" | "wasm-ld"))
        || !reads.executable(pin)?
    {
        return Err(Failure::Executable);
    }
    Ok(())
}

fn source_tree<P: ReadProvider>(
    directory: &Path,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    inventory: &mut Inventory,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    reads.checkpoint()?;
    if depth > MAX_DEPTH {
        return Err(Failure::Limit);
    }
    if !visited.insert(directory.to_owned()) {
        return Ok(());
    }
    if let Some(entries) = reads.output_directory(directory)? {
        inventory.push(CandidateRole::SourceWorktree, directory.to_owned())?;
        for entry in entries {
            if entry.is_directory() {
                source_tree(
                    &directory.join(entry.name()),
                    depth + 1,
                    visited,
                    inventory,
                    reads,
                )?;
            }
        }
    }
    Ok(())
}

fn static_package<P: ReadProvider>(
    manifest: &Value,
    directory: &Path,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    let package = manifest.get("package").ok_or(Failure::Manifest)?;
    // A build script can emit rustc-env (including TMPDIR) and linker/loader
    // directives after this analysis. Do not use its cached output as proof.
    match package.get("build") {
        Some(Value::Boolean(false)) => {}
        None => {
            if reads.read(&directory.join("build.rs"))?.is_some() {
                return Err(Failure::Environment);
            }
        }
        _ => return Err(Failure::Environment),
    }
    if package.get("links").is_some() {
        return Err(Failure::Environment);
    }
    if manifest
        .get("lib")
        .and_then(|lib| lib.get("proc-macro"))
        .is_some_and(|value| value.as_bool() != Some(false))
    {
        // Cargo's host-unit flag omission with explicit targets and macro
        // generated executable effects require a separate bounded model.
        return Err(Failure::Environment);
    }
    if manifest
        .get("lib")
        .and_then(|lib| lib.get("crate-type"))
        .and_then(Value::as_array)
        .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("proc-macro")))
    {
        return Err(Failure::Environment);
    }
    Ok(())
}

fn static_sources<P: ReadProvider>(
    sources: &Sources,
    inventory: &mut Inventory,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    let mut visited = BTreeSet::new();
    for directory in sources.directories() {
        // Cargo can update Cargo.lock in place. Directory-only storage v1
        // cannot certify an existing symlink or individually mounted source
        // file; retain a bounded no-follow metadata scan of each source root.
        reads.output_directory(directory)?.ok_or(Failure::Read)?;
        let bytes = reads
            .read(&directory.join("Cargo.toml"))?
            .ok_or(Failure::Manifest)?;
        let manifest: Value =
            toml::from_str(std::str::from_utf8(bytes).map_err(|_| Failure::Manifest)?)
                .map_err(|_| Failure::Manifest)?;
        let Some(package) = manifest.get("package") else {
            continue;
        };
        static_package(&manifest, directory, reads)?;
        for (key, path) in [
            ("autolib", "src"),
            ("autobins", "src/bin"),
            ("autoexamples", "examples"),
            ("autotests", "tests"),
            ("autobenches", "benches"),
        ] {
            if package
                .get(key)
                .is_none_or(|value| value.as_bool() == Some(true))
            {
                source_tree(&directory.join(path), 0, &mut visited, inventory, reads)?;
            } else if package.get(key).and_then(Value::as_bool) != Some(false) {
                return Err(Failure::Manifest);
            }
        }
        for key in ["lib", "bin", "example", "test", "bench"] {
            let Some(value) = manifest.get(key) else {
                continue;
            };
            let targets: Vec<&Value> = if key == "lib" {
                vec![value]
            } else {
                value.as_array().ok_or(Failure::Manifest)?.iter().collect()
            };
            for target in targets {
                for field in target.as_table().ok_or(Failure::Manifest)?.keys() {
                    if !matches!(
                        field.as_str(),
                        "name"
                            | "path"
                            | "test"
                            | "doctest"
                            | "bench"
                            | "doc"
                            | "harness"
                            | "proc-macro"
                            | "crate-type"
                            | "required-features"
                            | "edition"
                    ) {
                        return Err(Failure::Manifest);
                    }
                }
                if let Some(name) = target.get("name") {
                    let name = name.as_str().ok_or(Failure::Manifest)?;
                    if name.is_empty()
                        || !name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    {
                        return Err(Failure::Manifest);
                    }
                }
                if let Some(path) = target.get("path") {
                    let path = paths::absolute(
                        directory,
                        Path::new(path.as_str().ok_or(Failure::Manifest)?),
                    )?;
                    reads.read(&path)?.ok_or(Failure::Manifest)?;
                    source_tree(
                        path.parent().ok_or(Failure::Manifest)?,
                        0,
                        &mut visited,
                        inventory,
                        reads,
                    )?;
                }
            }
        }
    }
    reads.checkpoint()
}

fn loader(prefix: &[PathBuf], inherited: Option<&str>) -> Result<String, Failure> {
    let mut entries = prefix.to_vec();
    let existing: Vec<_> = inherited.map_or_else(Vec::new, |value| {
        value.split(':').map(PathBuf::from).collect()
    });
    if existing.starts_with(prefix) {
        entries = existing;
    } else {
        entries.extend(existing);
    }
    if entries.len() > 32 {
        return Err(Failure::Limit);
    }
    for path in &entries {
        paths::validate(path)?;
        if !path.is_absolute() || path.to_str().ok_or(Failure::Environment)?.contains(':') {
            return Err(Failure::Environment);
        }
    }
    std::env::join_paths(entries)
        .map_err(|_| Failure::Environment)?
        .into_string()
        .map_err(|_| Failure::Environment)
}

fn cargo_temporaries<P: ReadProvider>(
    cwd: &Path,
    model: &Model,
    inventory: &mut Inventory,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    // Cargo staging/temp operations use process inputs before compiler [env].
    // Nested clippy Cargo can have a different process environment as well.
    for environment in [
        model.process_environment(),
        model
            .nested_cargo_environment()
            .unwrap_or(model.process_environment()),
    ] {
        inventory.push(
            CandidateRole::BuildOutput,
            paths::absolute(cwd, Path::new(environment.get("TMPDIR").unwrap_or("/tmp")))?,
        )?;
    }
    inventory.expand(reads)
}

#[allow(clippy::too_many_arguments)]
fn generated<P: ReadProvider>(
    layout: &Layout,
    sources: &Sources,
    model: &Model,
    selection: &Selection,
    invocation: &Invocation,
    contract: &RustExecution,
    inventory: &mut Inventory,
    reads: &mut ReadSet<'_, P>,
) -> Result<[u8; 32], Failure> {
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-cargo-generated-inputs-v1\0");
    let host = layout.units().first().ok_or(Failure::Unsupported)?;
    let process = model
        .nested_cargo_environment()
        .unwrap_or(model.process_environment());
    // No build scripts means no native_dirs or extra_env additions. Cargo's
    // loader path uses its process environment, before child [env].
    let compiler_loader = loader(
        &[host.intermediate().join("deps")],
        process.get("LD_LIBRARY_PATH"),
    )?;
    let host_triple = contract
        .toolchain()
        .strip_prefix("1.94.0-")
        .ok_or(Failure::Toolchain)?;
    let cargo = contract
        .installed_executable("cargo")
        .ok_or(Failure::Executable)?;
    for unit in layout.units() {
        let runtime_loader = loader(
            &[
                unit.artifact().to_owned(),
                unit.intermediate().join("deps"),
                selection
                    .directory()
                    .join("lib/rustlib")
                    .join(unit.target().unwrap_or(host_triple))
                    .join("lib"),
            ],
            process.get("LD_LIBRARY_PATH"),
        )?;
        // Retain every package root and both workspace/dependency compiler
        // variants; Cargo sets child cwd to that package's root.
        for directory in sources.directories() {
            let mut child = model.child_environment().clone();
            child.cargo_generated("CARGO", cargo.to_str().ok_or(Failure::Executable)?)?;
            child.cargo_generated(
                "CARGO_MANIFEST_DIR",
                directory.to_str().ok_or(Failure::Manifest)?,
            )?;
            child.cargo_generated(
                "CARGO_MANIFEST_PATH",
                directory
                    .join("Cargo.toml")
                    .to_str()
                    .ok_or(Failure::Manifest)?,
            )?;
            child.cargo_generated(
                "CARGO_TARGET_TMPDIR",
                unit.temporary().to_str().ok_or(Failure::Environment)?,
            )?;
            let temp =
                paths::absolute(directory, Path::new(child.get("TMPDIR").unwrap_or("/tmp")))?;
            inventory.push(CandidateRole::CompilerTemporary, temp)?;
            let mut runtime = child.clone();
            runtime.cargo_generated("LD_LIBRARY_PATH", &runtime_loader)?;
            hash.update(runtime.digest());
            child.cargo_generated("LD_LIBRARY_PATH", &compiler_loader)?;
            hash.update(child.digest());
            if contract.is_proxy("rustc") {
                let primary_proxy = invocation.rustup_run() || contract.is_proxy("cargo");
                let clippy = invocation.action() == Action::Build(BuildAction::Clippy);
                let count = if clippy {
                    if primary_proxy { 3 } else { 2 }
                } else if primary_proxy {
                    2
                } else {
                    1
                };
                let compiler = rustup_environment(directory, &child, selection, contract, count)?;
                hash.update(compiler.digest());
            }
            hash.update(directory.as_os_str().as_encoded_bytes());
            reads.checkpoint()?;
        }
    }
    Ok(hash.finalize().into())
}

impl<'a, P: ReadProvider> Operation<'a, P> {
    /// Combine source, config, installed-tool and complete directory evidence.
    /// The cwd must come from the checked execution binding, never an unchecked
    /// client value. All observations use one original deadline.
    ///
    /// # Errors
    /// Unsupported dynamic configuration, sources/generated effects, incomplete
    /// outputs, changed reads or any shared limit/deadline failure deny.
    pub fn resolve(
        cwd: &Path,
        invocation: &Invocation,
        contract: &RustExecution,
        provider: &'a P,
        deadline: Instant,
    ) -> Result<Self, Failure> {
        let mut reads = ReadSet::new(provider, deadline);
        let selection = Selection::resolve(
            cwd,
            invocation,
            invocation.environment(),
            contract,
            &mut reads,
        )?;
        let options =
            if invocation.tool() == "cargo" && matches!(invocation.action(), Action::Build(_)) {
                Some(Options::extract(invocation, cwd)?)
            } else {
                None
            };
        let configuration = if let Some(options) = &options {
            Some(Configuration::load(
                cwd,
                Some(options.discovery_root(cwd)),
                invocation.environment(),
                options.configuration(),
                &mut reads,
            )?)
        } else {
            None
        };
        let model = Model::resolve(
            cwd,
            invocation,
            configuration.as_ref(),
            &selection,
            contract,
            &mut reads,
        )?;
        // Child selectors must agree as well, in the same read set/deadline.
        Selection::resolve(
            cwd,
            invocation,
            model.child_environment(),
            contract,
            &mut reads,
        )?;
        let mut generated_digest = [0; 32];
        let inventory = match invocation.action() {
            Action::AuditedReadOnly => None,
            Action::Build(BuildAction::Rustc | BuildAction::Rustdoc) => {
                if invocation.action() == Action::Build(BuildAction::Rustc) {
                    linker(invocation.arguments(), contract, &mut reads)?;
                }
                let mut inventory =
                    Inventory::direct(invocation, cwd, model.compiler_environment(), &mut reads)?;
                let mut visited = BTreeSet::new();
                for directory in inventory.source_roots() {
                    source_tree(&directory, 0, &mut visited, &mut inventory, &mut reads)?;
                }
                Some(inventory)
            }
            Action::Build(_) => {
                let options = options.as_ref().ok_or(Failure::Unsupported)?;
                let configuration = configuration.as_ref().ok_or(Failure::Configuration)?;
                linker(
                    &configuration.compiler_flags(invocation.environment(), BuildAction::Rustc)?,
                    contract,
                    &mut reads,
                )?;
                let sources = Sources::resolve(cwd, options.selection(), &mut reads)?;
                let layout = Layout::resolve(
                    options,
                    configuration,
                    &sources,
                    contract,
                    (model.process_environment(), model.compiler_environment()),
                    cwd,
                )?;
                let mut inventory = Inventory::cargo(&layout, &mut reads)?;
                cargo_temporaries(cwd, &model, &mut inventory, &mut reads)?;
                static_sources(&sources, &mut inventory, &mut reads)?;
                generated_digest = generated(
                    &layout,
                    &sources,
                    &model,
                    &selection,
                    invocation,
                    contract,
                    &mut inventory,
                    &mut reads,
                )?;
                Some(inventory)
            }
            Action::KnownNonExecution => return Err(Failure::Unsupported),
        };
        reads.recheck()?;
        Ok(Self {
            action: invocation.action(),
            model,
            inventory,
            reads,
            generated_digest,
            deadline,
        })
    }

    /// Exact classified static action. A build has all four required roles.
    #[must_use]
    pub const fn action(&self) -> Action {
        self.action
    }

    /// Complete role inventory; read-only operations deliberately have none.
    ///
    /// # Errors
    /// Bounded storage candidate construction failure denies.
    pub fn candidates(&self) -> Result<Vec<Candidate>, Failure> {
        self.inventory
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), Inventory::candidates)
    }

    /// Recheck all retained inputs and both protected executable inventories.
    /// Locality/binding/claim checks remain the consumer's separate obligation.
    ///
    /// # Errors
    /// Input replacement, missing evidence and the original deadline deny.
    pub fn recheck(&self, contract: &RustExecution) -> Result<(), Failure> {
        self.model.recheck_identities(contract, self.deadline)?;
        self.reads.recheck()
    }

    /// Domain-separated bounded operation/provenance digest, without raw data.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-complete-operation-v1\0");
        hash.update(self.model.digest());
        hash.update(self.reads.digest());
        hash.update(self.generated_digest);
        if let Some(inventory) = &self.inventory {
            hash.update(inventory.digest());
        }
        hash.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::config::execution::{RustExecution, pin_fixture_linker};
    use crate::execution_input::ExecutionInput;
    use crate::rust_build::command::{Invocation, parse};
    use crate::rust_build::files::fixtures::Scripted;
    use crate::storage::CandidateRole;

    use super::{Action, Failure, Operation, loader};

    fn execution_fixture(proxy: bool) -> (RustExecution, tempfile::TempDir) {
        let (mut contract, directory) = crate::config::execution::execution_fixture(proxy);
        pin_fixture_linker(&mut contract, directory.path());
        (contract, directory)
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn provider(contract: &RustExecution) -> Scripted {
        let provider = Scripted::default();
        let home = Path::new(contract.environment().get("RUSTUP_HOME").unwrap());
        provider.put(home.join("settings.toml").to_str().unwrap(),
            "version='12'\nauto_install='disable'\ndefault_toolchain='1.94.0-x86_64-unknown-linux-gnu'\n");
        provider.list(
            home.join("toolchains")
                .join(contract.toolchain())
                .to_str()
                .unwrap(),
            &[("bin", true), ("lib", true)],
        );
        for name in [
            "cargo",
            "rustc",
            "rustdoc",
            "env",
            "rustup",
            "cargo-clippy",
            "clippy-driver",
        ] {
            provider.put(
                contract.executable(name).unwrap().to_str().unwrap(),
                "PRIMARY_SENTINEL",
            );
            if let Some(installed) = contract.installed_executable(name) {
                provider.put(installed.to_str().unwrap(), "INSTALLED_SENTINEL");
            }
        }
        provider.put(
            contract.native_linker().unwrap().to_str().unwrap(),
            "LINKER_SENTINEL",
        );
        provider.put(
            "/work/Cargo.toml",
            "[package]\nname='literal'\nversion='1.0.0'",
        );
        provider.list("/work/src", &[("main.rs", false)]);
        provider.list("/work", &[("Cargo.toml", false), ("src", true)]);
        provider.put("/work/src/main.rs", "fn main() {}");
        provider
    }

    fn invocation(contract: &RustExecution, command: &str) -> Invocation {
        let command = if command.starts_with("rustc ")
            && !command.contains("--help")
            && !command.contains("--version")
        {
            format!(
                "{command} -C linker={} -C linker-flavor=ld.lld",
                contract.native_linker().unwrap().display()
            )
        } else {
            command.to_owned()
        };
        parse(
            &ExecutionInput::from_value(&json!({"command":command})).unwrap(),
            contract,
        )
        .unwrap()
    }

    #[test]
    fn every_action_combines_complete_roles_and_retains_nonexecution_evidence() {
        for proxy in [false, true] {
            let (contract, directory) = execution_fixture(proxy);
            for command in [
                "cargo build",
                "cargo check",
                "cargo test --no-run",
                "cargo test -- --list",
                "cargo clippy",
                "cargo run",
                "cargo bench -- --list",
                "cargo doc",
                "cargo install --path .",
                "cargo build --target=x86_64-unknown-linux-gnu --profile=release --timings",
                "cargo doc --target=wasm32-unknown-unknown",
                "rustc src/main.rs --out-dir /output",
                "rustc --test src/main.rs --out-dir /output",
                "rustdoc src/main.rs -o /output",
            ] {
                let provider = provider(&contract);
                let invocation = invocation(&contract, command);
                let operation = Operation::resolve(
                    Path::new("/work"),
                    &invocation,
                    &contract,
                    &provider,
                    deadline(),
                )
                .unwrap();
                let candidates = operation.candidates().unwrap();
                for role in [
                    CandidateRole::SourceWorktree,
                    CandidateRole::TargetDirectory,
                    CandidateRole::BuildOutput,
                    CandidateRole::CompilerTemporary,
                ] {
                    assert!(
                        candidates.iter().any(|candidate| candidate.role() == role),
                        "{command}"
                    );
                }
                assert!(candidates.len() <= 64);
                operation.recheck(&contract).unwrap();
                assert_ne!(operation.digest(), [0; 32]);
                assert!(!directory.path().join("marker").exists());
            }
        }
    }

    #[test]
    fn cargo_staging_temporaries_are_independent_of_forced_compiler_environment() {
        let (contract, _directory) = execution_fixture(true);
        for action in ["build", "clippy", "install --path ."] {
            let provider = provider(&contract);
            provider.put(
                "/work/.cargo/config",
                "[env]\nTMPDIR={value='/work/compiler-temp',force=true}\n",
            );
            provider.list("/work/cargo-stage", &[("nested", true)]);
            provider.list("/work/cargo-stage/nested", &[]);
            let invocation = invocation(
                &contract,
                &format!("TMPDIR=/work/cargo-stage cargo {action}"),
            );
            let operation = Operation::resolve(
                Path::new("/work"),
                &invocation,
                &contract,
                &provider,
                deadline(),
            )
            .unwrap();
            let inventory = operation.inventory.as_ref().unwrap();
            for path in ["/work/cargo-stage", "/work/cargo-stage/nested"] {
                assert!(inventory.contains(CandidateRole::BuildOutput, Path::new(path)));
            }
            assert!(inventory.contains(
                CandidateRole::CompilerTemporary,
                Path::new("/work/compiler-temp")
            ));
            operation.recheck(&contract).unwrap();
        }
    }

    #[test]
    fn readonly_keeps_executable_and_selector_rechecks_without_build_inventory() {
        let (contract, _directory) = execution_fixture(true);
        let provider = provider(&contract);
        for command in [
            "cargo --version",
            "cargo build --help",
            "rustc --help",
            "rustdoc --version",
        ] {
            let invocation = invocation(&contract, command);
            let operation = Operation::resolve(
                Path::new("/work"),
                &invocation,
                &contract,
                &provider,
                deadline(),
            )
            .unwrap();
            assert_eq!(operation.action(), Action::AuditedReadOnly);
            assert!(operation.candidates().unwrap().is_empty());
            operation.recheck(&contract).unwrap();
        }
    }

    #[test]
    fn unresolved_and_replaced_linkers_never_establish_complete_build() {
        let (contract, directory) = execution_fixture(false);
        for flags in [
            "",
            "-C linker=/opaque-secret -C linker-flavor=ld.lld",
            "-C linker-flavor=gcc",
            "-C linker-flavor=gnu-lld",
            "-C linker-flavor=wasm-lld",
            "-C link-arg=-o/opaque-secret",
        ] {
            let provider = provider(&contract);
            let input = ExecutionInput::from_value(
                &json!({"argv":["cargo", "build"], "env":{"RUSTFLAGS":flags}}),
            )
            .unwrap();
            let parsed = parse(&input, &contract).unwrap();
            assert!(
                Operation::resolve(
                    Path::new("/work"),
                    &parsed,
                    &contract,
                    &provider,
                    deadline()
                )
                .is_err()
            );
        }
        let provider = provider(&contract);
        let parsed = invocation(&contract, "cargo build");
        let operation = Operation::resolve(
            Path::new("/work"),
            &parsed,
            &contract,
            &provider,
            deadline(),
        )
        .unwrap();
        std::fs::write(
            contract.native_linker().unwrap(),
            "REPLACED_LINKER_SENTINEL",
        )
        .unwrap();
        assert!(operation.recheck(&contract).is_err());
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn generated_build_script_and_declared_source_changes_cannot_claim_completeness() {
        let (contract, directory) = execution_fixture(true);
        let invocation = invocation(&contract, "cargo build");
        for manifest in [
            "[package]\nname='literal'\nversion='1.0.0'\nbuild='secret-script.rs'",
            "[package]\nname='literal'\nversion='1.0.0'\nlinks='secret-native'",
            "[package]\nname='literal'\nversion='1.0.0'\n[[bin]]\nname='literal'\npath='/unavailable/source.rs'",
            "[package]\nname='literal'\nversion='1.0.0'\n[[bin]]\nname='literal'\nfilename='secret-output'",
        ] {
            let provider = provider(&contract);
            provider.put("/work/Cargo.toml", manifest);
            assert!(
                Operation::resolve(
                    Path::new("/work"),
                    &invocation,
                    &contract,
                    &provider,
                    deadline()
                )
                .is_err()
            );
        }
        let provider = provider(&contract);
        provider.put(
            "/work/build.rs",
            "panic!(\"SECRET_SENTINEL: never execute\");",
        );
        assert_eq!(
            Operation::resolve(
                Path::new("/work"),
                &invocation,
                &contract,
                &provider,
                deadline()
            )
            .err(),
            Some(Failure::Environment)
        );
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn new_default_script_source_directory_and_manifest_replacements_invalidate() {
        let (contract, _directory) = execution_fixture(true);
        let invocation = invocation(&contract, "cargo build");
        for changed in ["script", "directory", "manifest"] {
            let provider = provider(&contract);
            let operation = Operation::resolve(
                Path::new("/work"),
                &invocation,
                &contract,
                &provider,
                deadline(),
            )
            .unwrap();
            match changed {
                "script" => provider.put("/work/build.rs", "secret"),
                "directory" => provider.list("/work/src", &[("main.rs", false), ("mounted", true)]),
                _ => provider.put(
                    "/work/Cargo.toml",
                    "[package]\nname='replaced'\nversion='1.0.0'",
                ),
            }
            assert_eq!(operation.recheck(&contract), Err(Failure::Changed));
        }
    }

    #[test]
    fn source_root_metadata_is_required_and_new_lock_entries_invalidate() {
        let (contract, _directory) = execution_fixture(true);
        for command in [
            "cargo build",
            "rustc src/main.rs --out-dir /out",
            "rustdoc src/main.rs -o /out",
        ] {
            let invocation = invocation(&contract, command);
            let provider = provider(&contract);
            let operation = Operation::resolve(
                Path::new("/work"),
                &invocation,
                &contract,
                &provider,
                deadline(),
            )
            .unwrap();
            let root = if command.starts_with("cargo") {
                "/work"
            } else {
                "/work/src"
            };
            provider.list(root, &[("Cargo.lock", false), ("main.rs", false)]);
            assert_eq!(operation.recheck(&contract), Err(Failure::Changed));
            provider.directories.borrow_mut().remove(Path::new(root));
            assert_eq!(
                Operation::resolve(
                    Path::new("/work"),
                    &invocation,
                    &contract,
                    &provider,
                    deadline()
                )
                .err(),
                Some(Failure::Read)
            );
        }
    }

    #[test]
    fn proc_macro_crate_type_cannot_bypass_generated_environment_exclusion() {
        let (contract, _directory) = execution_fixture(true);
        let provider = provider(&contract);
        provider.put("/work/Cargo.toml", "[package]\nname='literal'\nversion='1.0.0'\nbuild=false\n[lib]\ncrate-type=['proc-macro']\n");
        assert_eq!(
            Operation::resolve(
                Path::new("/work"),
                &invocation(&contract, "cargo build"),
                &contract,
                &provider,
                deadline()
            )
            .err(),
            Some(Failure::Environment)
        );
    }

    #[test]
    fn loader_prefix_rule_and_encoding_are_exact_and_bounded() {
        let prefix = vec!["/host/deps".into()];
        assert_eq!(
            loader(&prefix, Some("/rustup/lib")).unwrap(),
            "/host/deps:/rustup/lib"
        );
        assert_eq!(
            loader(&prefix, Some("/host/deps:/rustup/lib")).unwrap(),
            "/host/deps:/rustup/lib"
        );
        assert!(loader(&["/colon:path".into()], None).is_err());
        assert!(loader(&prefix, Some("relative")).is_err());
        assert!(loader(&prefix, Some("")).is_err());
    }
}
