//! Exact entrypoint/installed mapping and rustup-generated process environment.
//! This read-only model is not locality proof or a complete build decision.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::config::execution::RustExecution;

use super::command::Invocation;
use super::config::Configuration;
use super::environment::Environment;
use super::files::{ReadProvider, ReadSet};
use super::toolchain::Selection;
use super::{Action, BuildAction, Failure, paths};

fn physical(path: &Path) -> Result<(), Failure> {
    paths::validate(path)?;
    if !path.is_absolute()
        || path
            .to_str()
            .ok_or(Failure::Executable)?
            .split('/')
            .any(|part| matches!(part, "." | ".."))
    {
        return Err(Failure::Executable);
    }
    Ok(())
}

fn search<P: ReadProvider>(
    word: &str,
    expected: &Path,
    environment: &Environment,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    physical(expected)?;
    if Path::new(word).is_absolute() {
        if Path::new(word) != expected || !reads.executable(expected)? {
            return Err(Failure::Executable);
        }
        return Ok(());
    }
    if word.is_empty() || word.contains('/') {
        return Err(Failure::Executable);
    }
    let value = environment.get("PATH").ok_or(Failure::Executable)?;
    for (index, directory) in value.split(':').enumerate() {
        if index >= 32 {
            return Err(Failure::Limit);
        }
        physical(Path::new(directory))?;
        let candidate = Path::new(directory).join(word);
        if reads.executable(&candidate)? {
            return if candidate == expected {
                Ok(())
            } else {
                Err(Failure::Executable)
            };
        }
    }
    Err(Failure::Executable)
}

fn cargo_home(cwd: &Path, environment: &Environment) -> Result<PathBuf, Failure> {
    let home = if let Some(value) = environment
        .get("CARGO_HOME")
        .filter(|value| !value.is_empty())
    {
        paths::absolute(cwd, Path::new(value))?
    } else {
        let value = environment.get("HOME").ok_or(Failure::Environment)?;
        paths::absolute(cwd, Path::new(value))?.join(".cargo")
    };
    physical(&home)?;
    Ok(home)
}

// Rustup 1.28.2 insert_path preserves an existing entry's position instead of
// moving it to the front. The entire resulting search is checked separately.
fn unique_path(value: Option<&str>, prepend: &Path) -> Result<String, Failure> {
    physical(prepend)?;
    let prepend = prepend.to_str().ok_or(Failure::Environment)?;
    if prepend.contains(':') {
        return Err(Failure::Environment);
    }
    let Some(value) = value else {
        return Ok(prepend.to_owned());
    };
    for directory in value.split(':') {
        physical(Path::new(directory))?;
    }
    if value
        .split(':')
        .any(|directory| Path::new(directory) == Path::new(prepend))
    {
        Ok(value.to_owned())
    } else {
        Ok(format!("{prepend}:{value}"))
    }
}

fn rustup_environment(
    cwd: &Path,
    environment: &Environment,
    selection: &Selection,
    contract: &RustExecution,
    count: u8,
) -> Result<Environment, Failure> {
    if !(1..=3).contains(&count) {
        return Err(Failure::Environment);
    }
    // These keys are forbidden in inherited/client/wrapper/config input.
    // Only our own already-modeled first rustup hop may reach a second hop.
    if count == 1
        && (environment.get("LD_LIBRARY_PATH").is_some()
            || environment.get("RUST_RECURSION_COUNT").is_some())
    {
        return Err(Failure::Environment);
    }
    if count > 1
        && environment.get("RUST_RECURSION_COUNT") != Some((count - 1).to_string().as_str())
    {
        return Err(Failure::Environment);
    }
    let mut generated = environment.clone();
    let home = cargo_home(cwd, environment)?;
    let path = unique_path(environment.get("PATH"), &home.join("bin"))?;
    let loader = unique_path(
        environment.get("LD_LIBRARY_PATH"),
        &selection.directory().join("lib"),
    )?;
    let rustup = selection
        .directory()
        .parent()
        .and_then(Path::parent)
        .ok_or(Failure::Toolchain)?;
    let recursion = count.to_string();
    for (key, value) in [
        ("PATH", path.as_str()),
        ("LD_LIBRARY_PATH", loader.as_str()),
        ("CARGO_HOME", home.to_str().ok_or(Failure::Environment)?),
        ("RUSTUP_HOME", rustup.to_str().ok_or(Failure::Environment)?),
        ("RUSTUP_TOOLCHAIN", contract.toolchain()),
        ("RUST_RECURSION_COUNT", recursion.as_str()),
    ] {
        generated.generated(key, value)?;
    }
    Ok(generated)
}

fn invocation_digest(cwd: &Path, invocation: &Invocation) -> [u8; 32] {
    let mut invocation_hash = Sha256::new();
    invocation_hash.update(b"memory-hooks-rust-invocation-v1\0");
    invocation_hash.update(invocation.tool().len().to_be_bytes());
    invocation_hash.update(invocation.tool().as_bytes());
    invocation_hash.update([u8::from(invocation.explicit_toolchain())]);
    for entry in invocation.entries() {
        for part in [entry.name.as_bytes(), entry.word.as_bytes()] {
            invocation_hash.update(part.len().to_be_bytes());
            invocation_hash.update(part);
        }
        invocation_hash.update(entry.environment.digest());
    }
    for word in invocation.arguments() {
        invocation_hash.update(word.len().to_be_bytes());
        invocation_hash.update(word.as_bytes());
    }
    invocation_hash.update(cwd.as_os_str().as_encoded_bytes());
    invocation_hash.finalize().into()
}

fn cargo_child<P: ReadProvider>(
    config: &Configuration,
    process: &Environment,
    inherited: &Environment,
    contract: &RustExecution,
    reads: &mut ReadSet<'_, P>,
) -> Result<Environment, Failure> {
    config.validate_execution(inherited, contract)?;
    let child = config.child_environment(process)?;
    for key in [
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "LD_LIBRARY_PATH",
        "RUST_RECURSION_COUNT",
        "CARGO",
        "CLIPPY_ARGS",
    ] {
        if child.get(key) != process.get(key) {
            return Err(Failure::Environment);
        }
    }
    if process.origin("RUSTC_WORKSPACE_WRAPPER") == Some(super::environment::Origin::Clippy)
        && child.get("RUSTC_WORKSPACE_WRAPPER") != process.get("RUSTC_WORKSPACE_WRAPPER")
    {
        return Err(Failure::Executable);
    }
    for name in ["rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
        search(
            name,
            contract.executable(name).ok_or(Failure::Executable)?,
            &child,
            reads,
        )?;
    }
    Ok(child)
}

fn clippy_process<P: ReadProvider>(
    cwd: &Path,
    process: &Environment,
    selection: &Selection,
    contract: &RustExecution,
    reads: &mut ReadSet<'_, P>,
    proxy: bool,
) -> Result<Environment, Failure> {
    let mut extension = process.clone();
    let cargo = contract
        .installed_executable("cargo")
        .ok_or(Failure::Executable)?;
    extension.cargo_generated("CARGO", cargo.to_str().ok_or(Failure::Executable)?)?;
    let mut search_environment = process.clone();
    // Cargo's external search has its own unique Cargo-home/bin insertion,
    // even when Cargo was launched directly instead of through a proxy.
    let path = unique_path(process.get("PATH"), &cargo_home(cwd, process)?.join("bin"))?;
    search_environment.generated("PATH", &path)?;
    search(
        "cargo-clippy",
        contract
            .executable("cargo-clippy")
            .ok_or(Failure::Executable)?,
        &search_environment,
        reads,
    )?;
    if contract.is_proxy("cargo-clippy") {
        extension = rustup_environment(
            cwd,
            &extension,
            selection,
            contract,
            if proxy { 2 } else { 1 },
        )?;
    }
    let driver = contract
        .installed_executable("clippy-driver")
        .ok_or(Failure::Executable)?;
    extension.clippy_generated(
        "RUSTC_WORKSPACE_WRAPPER",
        driver.to_str().ok_or(Failure::Executable)?,
    )?;
    // The grammar rejects passthrough/fix/no-deps forms; pinned cargo-clippy
    // supplies an empty CLIPPY_ARGS and an installed sibling driver here.
    extension.clippy_generated("CLIPPY_ARGS", "")?;
    Ok(extension)
}

fn verify_entries<P: ReadProvider>(
    invocation: &Invocation,
    selection: &Selection,
    contract: &RustExecution,
    reads: &mut ReadSet<'_, P>,
) -> Result<(), Failure> {
    for entry in invocation.entries() {
        search(
            &entry.word,
            contract
                .executable(&entry.name)
                .ok_or(Failure::Executable)?,
            &entry.environment,
            reads,
        )?;
    }
    for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
        let installed = selection.directory().join("bin").join(name);
        if contract.installed_executable(name) != Some(installed.as_path())
            || !reads.executable(&installed)?
        {
            return Err(Failure::Executable);
        }
    }
    Ok(())
}

/// Private static execution model, with separate Cargo and compiler inputs.
/// It supplies no policy, locality, claim or candidate-completeness authority.
pub struct Model {
    process: Environment,
    child: Environment,
    compiler: Environment,
    directory: PathBuf,
    proxy: bool,
    invocation_digest: [u8; 32],
    nested_cargo: Option<Environment>,
    dependency_compiler: Environment,
}

impl Model {
    /// Resolve the installed pin and every modeled primary/secondary search.
    /// Callers must retain/recheck the shared read set and separately verify
    /// locality, binding, installed identities and the exact claimed pack.
    ///
    /// # Errors
    /// Ambiguous search/aliases, missing tools, environment replacement, unknown
    /// configuration and shared budget exhaustion deny without launching tools.
    pub fn resolve<P: ReadProvider>(
        cwd: &Path,
        invocation: &Invocation,
        config: Option<&Configuration>,
        selection: &Selection,
        contract: &RustExecution,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        physical(cwd)?;
        verify_entries(invocation, selection, contract, reads)?;
        let proxy = invocation.rustup_run() || contract.is_proxy(invocation.tool());
        if invocation.explicit_toolchain() && !proxy {
            // Installed Cargo does not interpret the rustup proxy's + selector.
            return Err(Failure::Toolchain);
        }
        let process = if proxy {
            rustup_environment(cwd, invocation.environment(), selection, contract, 1)?
        } else {
            invocation.environment().clone()
        };
        let cargo_build =
            invocation.tool() == "cargo" && matches!(invocation.action(), Action::Build(_));
        let clippy = invocation.action() == Action::Build(BuildAction::Clippy);
        if clippy
            && invocation
                .arguments()
                .first()
                .is_none_or(|word| word != "clippy")
        {
            return Err(Failure::Unsupported);
        }
        let nested_cargo = if clippy {
            Some(clippy_process(
                cwd, &process, selection, contract, reads, proxy,
            )?)
        } else {
            None
        };
        let cargo_process = nested_cargo.as_ref().unwrap_or(&process);
        let child = if cargo_build {
            cargo_child(
                config.ok_or(Failure::Configuration)?,
                cargo_process,
                invocation.environment(),
                contract,
                reads,
            )?
        } else {
            if config.is_some() {
                return Err(Failure::Configuration);
            }
            process.clone()
        };
        let compiler = if cargo_build && !clippy && contract.is_proxy("rustc") {
            rustup_environment(cwd, &child, selection, contract, if proxy { 2 } else { 1 })?
        } else {
            child.clone()
        };
        let dependency_compiler = if clippy && contract.is_proxy("rustc") {
            rustup_environment(cwd, &child, selection, contract, if proxy { 3 } else { 2 })?
        } else {
            compiler.clone()
        };
        reads.checkpoint()?;
        Ok(Self {
            process,
            child,
            compiler,
            directory: selection.directory().to_owned(),
            proxy,
            invocation_digest: invocation_digest(cwd, invocation),
            nested_cargo,
            dependency_compiler,
        })
    }

    /// Recheck primary and installed content identities without executing them.
    /// Locality and read-set checks remain mandatory adjacent to final completion.
    ///
    /// # Errors
    /// Changed, inaccessible, untrusted or late executable evidence denies.
    pub fn recheck_identities(
        &self,
        contract: &RustExecution,
        deadline: Instant,
    ) -> Result<(), Failure> {
        contract
            .recheck_executables(deadline)
            .map_err(|_| Failure::Executable)?;
        contract
            .verify_installed_toolchain(&self.directory, deadline)
            .map_err(|_| Failure::Executable)
    }

    /// Cargo's own process inputs, including fixed rustup-generated values.
    #[must_use]
    pub const fn process_environment(&self) -> &Environment {
        &self.process
    }

    /// Cargo child inputs after [env], without feeding them back into Cargo.
    #[must_use]
    pub const fn child_environment(&self) -> &Environment {
        &self.child
    }

    /// Compiler inputs after a supported secondary rustup/Clippy hop, before
    /// Cargo's layout-derived loader additions (resolved by the output stage).
    #[must_use]
    pub const fn compiler_environment(&self) -> &Environment {
        &self.compiler
    }

    /// Clippy's secondary installed Cargo process, when that action applies.
    #[must_use]
    pub const fn nested_cargo_environment(&self) -> Option<&Environment> {
        self.nested_cargo.as_ref()
    }

    /// Non-workspace dependency compiler inputs; Clippy's driver runs in-process
    /// for workspace packages, while other dependencies can use a rustc proxy.
    #[must_use]
    pub const fn dependency_compiler_environment(&self) -> &Environment {
        &self.dependency_compiler
    }

    /// Domain-separated digest of bounded private execution values/provenance.
    /// Hashes are not encryption and may expose guesses of low-entropy inputs.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-execution-model-v1\0");
        hash.update([u8::from(self.proxy)]);
        hash.update(self.invocation_digest);
        for (index, environment) in [
            &self.process,
            &self.child,
            &self.compiler,
            &self.dependency_compiler,
        ]
        .into_iter()
        .enumerate()
        {
            hash.update(index.to_be_bytes());
            hash.update(environment.digest());
        }
        hash.update([u8::from(self.nested_cargo.is_some())]);
        if let Some(environment) = &self.nested_cargo {
            hash.update(environment.digest());
        }
        hash.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::config::execution::{RustExecution, execution_fixture};
    use crate::execution_input::ExecutionInput;
    use crate::rust_build::command::{self, Invocation};
    use crate::rust_build::config::Configuration;
    use crate::rust_build::environment::Origin;
    use crate::rust_build::files::fixtures::Scripted;
    use crate::rust_build::files::{MAX_FILES, ReadSet};
    use crate::rust_build::toolchain::Selection;

    use super::{Failure, Model, unique_path};

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn provider(contract: &RustExecution) -> Scripted {
        let provider = Scripted::default();
        let home = Path::new(contract.environment().get("RUSTUP_HOME").unwrap());
        provider.put(home.join("settings.toml").to_str().unwrap(), "version='12'\nauto_install='disable'\ndefault_toolchain='1.94.0-x86_64-unknown-linux-gnu'\n");
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
        provider
    }

    fn parse(contract: &RustExecution, source: &str) -> Invocation {
        command::parse(
            &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
            contract,
        )
        .unwrap()
    }

    fn resolve(
        contract: &RustExecution,
        provider: &Scripted,
        source: &str,
        config_text: Option<&str>,
    ) -> Result<Model, Failure> {
        if let Some(text) = config_text {
            provider.put("/work/.cargo/config.toml", text);
        }
        let invocation = parse(contract, source);
        let mut reads = ReadSet::new(provider, deadline());
        let selection = Selection::resolve(
            Path::new("/work"),
            &invocation,
            invocation.environment(),
            contract,
            &mut reads,
        )?;
        let config = if invocation.tool() == "cargo"
            && matches!(invocation.action(), crate::rust_build::Action::Build(_))
        {
            Some(Configuration::load(
                Path::new("/work"),
                Some(Path::new("/work")),
                invocation.environment(),
                &[],
                &mut reads,
            )?)
        } else {
            None
        };
        let model = Model::resolve(
            Path::new("/work"),
            &invocation,
            config.as_ref(),
            &selection,
            contract,
            &mut reads,
        )?;
        reads.recheck()?;
        Ok(model)
    }

    #[test]
    fn proxy_process_child_and_compiler_environments_keep_distinct_provenance() {
        let (contract, directory) = execution_fixture(true);
        let provider = provider(&contract);
        let model = resolve(&contract, &provider, "cargo build", Some("[env]\nTMPDIR={value='compiler tmp',force=true,relative=true}\nCARGO_TARGET_DIR={value='child-target',force=true}")).unwrap();
        let process = model.process_environment();
        assert_eq!(
            process.get("PATH"),
            contract.environment().get("PATH").map(String::as_str)
        );
        assert_eq!(process.origin("PATH"), Some(Origin::Rustup));
        assert_eq!(process.get("RUST_RECURSION_COUNT"), Some("1"));
        assert_eq!(process.get("RUSTUP_TOOLCHAIN"), Some(contract.toolchain()));
        assert_eq!(process.origin("LD_LIBRARY_PATH"), Some(Origin::Rustup));
        assert_eq!(process.get("CARGO_TARGET_DIR"), None);
        assert_eq!(
            model.child_environment().get("CARGO_TARGET_DIR"),
            Some("child-target")
        );
        assert_eq!(
            model.child_environment().get("TMPDIR"),
            Some("/work/compiler tmp")
        );
        assert_eq!(
            model.compiler_environment().get("TMPDIR"),
            Some("/work/compiler tmp")
        );
        assert_eq!(
            model.compiler_environment().get("RUST_RECURSION_COUNT"),
            Some("2")
        );
        assert_eq!(
            model.compiler_environment().get("LD_LIBRARY_PATH"),
            process.get("LD_LIBRARY_PATH")
        );
        model.recheck_identities(&contract, deadline()).unwrap();
        assert_ne!(model.digest(), [0; 32]);
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn installed_entrypoints_and_rustup_run_have_different_environment_effects() {
        let (contract, directory) = execution_fixture(false);
        let provider = provider(&contract);
        let direct = resolve(&contract, &provider, "cargo build", None).unwrap();
        assert_eq!(
            direct.process_environment().get("RUST_RECURSION_COUNT"),
            None
        );
        assert_eq!(direct.compiler_environment().get("LD_LIBRARY_PATH"), None);
        assert!(resolve(&contract, &provider, "cargo +1.94.0 build", None).is_err());
        let wrapped = resolve(&contract, &provider, "rustup run 1.94.0 cargo build", None).unwrap();
        assert_eq!(
            wrapped.process_environment().get("RUST_RECURSION_COUNT"),
            Some("1")
        );
        assert!(
            wrapped
                .process_environment()
                .get("PATH")
                .unwrap()
                .starts_with(contract.environment().get("CARGO_HOME").unwrap())
        );
        for name in ["cargo", "rustc", "rustdoc"] {
            let source = format!(
                "rustup run 1.94.0 {} --version",
                contract.executable(name).unwrap().display()
            );
            assert_eq!(
                command::parse(
                    &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
                    &contract
                )
                .err(),
                Some(Failure::Unsupported)
            );
        }
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn rustup_unique_path_keeps_existing_position_and_denies_logical_aliases() {
        assert_eq!(
            unique_path(None, Path::new("/proxy:alias/bin")),
            Err(Failure::Environment)
        );
        assert_eq!(
            unique_path(Some("/early:/proxy/bin:/late"), Path::new("/proxy/bin")).unwrap(),
            "/early:/proxy/bin:/late"
        );
        assert_eq!(
            unique_path(Some("/early:/late"), Path::new("/proxy/bin")).unwrap(),
            "/proxy/bin:/early:/late"
        );
        assert_eq!(
            unique_path(None, Path::new("/proxy/bin")).unwrap(),
            "/proxy/bin"
        );
        assert_eq!(
            unique_path(Some("/early:/proxy/bin/:/late"), Path::new("/proxy/bin")).unwrap(),
            "/early:/proxy/bin/:/late"
        );
        for path in ["", "/good:", "relative", "/a/../b", "/a/./b"] {
            assert!(unique_path(Some(path), Path::new("/proxy/bin")).is_err());
        }
    }

    #[test]
    fn all_actions_direct_compilers_readonly_and_literal_env_wrappers_are_nonexecuting() {
        let (contract, directory) = execution_fixture(true);
        let provider = provider(&contract);
        for source in [
            "cargo build",
            "cargo check",
            "cargo test --no-run",
            "cargo clippy",
            "cargo run",
            "cargo bench -- --list",
            "cargo doc",
            "cargo install --path /work",
            "cargo +1.94.0 build",
            "rustup run 1.94.0 cargo build",
            "rustc source.rs --out-dir /output",
            "rustc --test source.rs --out-dir /output",
            "rustdoc source.rs -o /documentation",
            "cargo --version",
            "cargo build --help",
            "rustc --version",
            "rustdoc --help",
        ] {
            let model = resolve(&contract, &provider, source, None).unwrap();
            model.recheck_identities(&contract, deadline()).unwrap();
        }
        let mut argv = vec!["env".to_owned(), "-i".to_owned()];
        argv.extend(
            contract
                .environment()
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        argv.extend(["cargo".to_owned(), "build".to_owned()]);
        let invocation = command::parse(
            &ExecutionInput::from_value(&json!({"argv":argv})).unwrap(),
            &contract,
        )
        .unwrap();
        let mut reads = ReadSet::new(&provider, deadline());
        let selection = Selection::resolve(
            Path::new("/work"),
            &invocation,
            invocation.environment(),
            &contract,
            &mut reads,
        )
        .unwrap();
        let config = Configuration::load(
            Path::new("/work"),
            Some(Path::new("/work")),
            invocation.environment(),
            &[],
            &mut reads,
        )
        .unwrap();
        Model::resolve(
            Path::new("/work"),
            &invocation,
            Some(&config),
            &selection,
            &contract,
            &mut reads,
        )
        .unwrap();
        reads.recheck().unwrap();
        let plain = resolve(&contract, &provider, "cargo build", None).unwrap();
        let unset = resolve(&contract, &provider, "env -u NO_COLOR cargo build", None).unwrap();
        assert_eq!(
            plain.process_environment().values(),
            unset.process_environment().values()
        );
        assert_ne!(plain.digest(), unset.digest());
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn clippy_secondary_cargo_driver_and_dependency_proxy_have_separate_inputs() {
        for proxy in [false, true] {
            let (contract, directory) = execution_fixture(proxy);
            let provider = provider(&contract);
            let model = resolve(
                &contract,
                &provider,
                "cargo clippy",
                Some("[env]\nTMPDIR={value='compiler tmp',force=true,relative=true}"),
            )
            .unwrap();
            let nested = model.nested_cargo_environment().unwrap();
            assert_eq!(
                nested.get("CARGO"),
                contract.installed_executable("cargo").unwrap().to_str()
            );
            assert_eq!(
                nested.get("TMPDIR"),
                contract.environment().get("TMPDIR").map(String::as_str)
            );
            assert_eq!(nested.origin("CARGO"), Some(Origin::Cargo));
            assert_eq!(
                nested.get("RUSTC_WORKSPACE_WRAPPER"),
                contract
                    .installed_executable("clippy-driver")
                    .unwrap()
                    .to_str()
            );
            assert_eq!(
                nested.origin("RUSTC_WORKSPACE_WRAPPER"),
                Some(Origin::Clippy)
            );
            assert_eq!(nested.get("CLIPPY_ARGS"), Some(""));
            assert_eq!(
                model.compiler_environment().get("TMPDIR"),
                Some("/work/compiler tmp")
            );
            assert_eq!(
                model.compiler_environment().get("RUST_RECURSION_COUNT"),
                if proxy { Some("2") } else { None }
            );
            assert_eq!(
                model
                    .dependency_compiler_environment()
                    .get("RUST_RECURSION_COUNT"),
                if proxy { Some("3") } else { None }
            );
            assert!(resolve(&contract, &provider, "cargo --offline clippy", None).is_err());
            model.recheck_identities(&contract, deadline()).unwrap();
            assert!(!directory.path().join("marker").exists());
        }
        let (contract, _directory) = execution_fixture(true);
        let provider = provider(&contract);
        assert!(
            resolve(
                &contract,
                &provider,
                "cargo clippy",
                Some("[env]\nRUSTC_WORKSPACE_WRAPPER={value='',force=true}")
            )
            .is_err()
        );
    }

    #[test]
    fn earlier_search_absence_and_installed_replacement_join_final_read_set() {
        let (contract, directory) = execution_fixture(true);
        for replaced in [
            directory.path().join("search-empty/rustc"),
            contract.installed_executable("rustc").unwrap().to_owned(),
        ] {
            let provider = provider(&contract);
            let invocation = parse(&contract, "cargo build");
            let mut reads = ReadSet::new(&provider, deadline());
            let selection = Selection::resolve(
                Path::new("/work"),
                &invocation,
                invocation.environment(),
                &contract,
                &mut reads,
            )
            .unwrap();
            let config = Configuration::load(
                Path::new("/work"),
                Some(Path::new("/work")),
                invocation.environment(),
                &[],
                &mut reads,
            )
            .unwrap();
            let model = Model::resolve(
                Path::new("/work"),
                &invocation,
                Some(&config),
                &selection,
                &contract,
                &mut reads,
            )
            .unwrap();
            reads.recheck().unwrap();
            provider.put(replaced.to_str().unwrap(), "REPLACEMENT_SECRET_SENTINEL");
            let error = reads.recheck().unwrap_err();
            assert_eq!(error, Failure::Changed);
            assert!(!format!("{error:?}").contains("REPLACEMENT_SECRET_SENTINEL"));
            assert_ne!(model.digest(), [0; 32]);
        }
    }

    #[test]
    fn protected_proxy_and_installed_content_are_rechecked_independently() {
        for installed in [false, true] {
            let (contract, directory) = execution_fixture(true);
            let provider = provider(&contract);
            let model = resolve(&contract, &provider, "cargo build", None).unwrap();
            model.recheck_identities(&contract, deadline()).unwrap();
            let path = if installed {
                contract.installed_executable("rustc")
            } else {
                contract.executable("rustc")
            }
            .unwrap();
            std::fs::write(path, "REPLACEMENT_SECRET_SENTINEL").unwrap();
            assert_eq!(
                model.recheck_identities(&contract, deadline()),
                Err(Failure::Executable)
            );
            assert!(!directory.path().join("marker").exists());
        }
    }

    #[test]
    fn unknown_generated_inputs_child_mechanisms_and_shared_limits_deny() {
        let (contract, _directory) = execution_fixture(true);
        for key in ["LD_LIBRARY_PATH", "LD_PRELOAD", "RUST_RECURSION_COUNT"] {
            let input = ExecutionInput::from_value(
                &json!({"command":"cargo build", "env":{key:"SECRET_SENTINEL"}}),
            )
            .unwrap();
            assert_eq!(
                command::parse(&input, &contract).err(),
                Some(Failure::Environment)
            );
        }
        for text in [
            "[env]\nPATH={value='/other',force=true}",
            "[env]\nHOME={value='/other',force=true}",
            "[env]\nLD_LIBRARY_PATH='SECRET_SENTINEL'",
        ] {
            let provider = provider(&contract);
            assert!(resolve(&contract, &provider, "cargo build", Some(text)).is_err());
        }
        let provider = provider(&contract);
        let invocation = parse(&contract, "cargo build");
        let mut reads = ReadSet::new(&provider, deadline());
        let selection = Selection::resolve(
            Path::new("/work"),
            &invocation,
            invocation.environment(),
            &contract,
            &mut reads,
        )
        .unwrap();
        let config = Configuration::load(
            Path::new("/work"),
            Some(Path::new("/work")),
            invocation.environment(),
            &[],
            &mut reads,
        )
        .unwrap();
        for index in 0..MAX_FILES {
            if reads
                .read(&Path::new("/absent").join(index.to_string()))
                .is_err()
            {
                break;
            }
        }
        assert_eq!(
            Model::resolve(
                Path::new("/work"),
                &invocation,
                Some(&config),
                &selection,
                &contract,
                &mut reads
            )
            .err(),
            Some(Failure::Limit)
        );
        let mut expired = ReadSet::new(
            &provider,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        );
        assert_eq!(
            Model::resolve(
                Path::new("/work"),
                &invocation,
                Some(&config),
                &selection,
                &contract,
                &mut expired
            )
            .err(),
            Some(Failure::Deadline)
        );
    }
}
