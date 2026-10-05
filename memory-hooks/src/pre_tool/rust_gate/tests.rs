//! Claim-locked acceptance with test-only read/backing/locality boundaries.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use memory_common::guardrails::GuardrailPack;
use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Check, Runtime};
use crate::config::ClientAdapter;
use crate::config::execution::{RustExecution, execution_fixture, pin_fixture_linker};
use crate::error::{Error, Result};
use crate::execution_input::ExecutionInput;
use crate::installation::Installation;
use crate::pre_tool::{PreToolEvent, SpawnKind, ToolCategory};
use crate::rust_build::Failure as AnalysisFailure;
use crate::rust_build::files::{Filesystem, Observation, ReadProvider};
use crate::session_identity::ChildIdentity;
use crate::snapshot::ClaimedCheck;
use crate::storage::{self, Candidate, CandidateRole, Report};

struct Rig {
    tools: tempfile::TempDir,
    root: tempfile::TempDir,
    installation: Installation,
    cwd: PathBuf,
    pack: GuardrailPack,
    children: Cell<usize>,
    listener: TcpListener,
}

#[test]
fn late_durable_audit_cannot_complete_original_deadline() {
    let persisted = Cell::new(false);
    let result = super::audit_before(Instant::now() + Duration::from_secs(1), || {
        persisted.set(true);
        std::thread::sleep(Duration::from_millis(1100));
        Ok(())
    });
    assert!(persisted.get());
    assert!(matches!(result, Err(Error::RustAnalysisIndeterminate)));
    assert!(super::audit_before(Instant::now() + Duration::from_secs(5), || Ok(())).is_ok());
    assert!(matches!(
        super::audit_before(Instant::now() + Duration::from_secs(5), || Err(
            Error::StateCorrupt
        )),
        Err(Error::AuditFailed)
    ));
}

impl Rig {
    fn new(adapter: ClientAdapter) -> Self {
        Self::with_contract(adapter, true, false)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one protected v6 fixture shared by the claim/storage acceptance matrix"
    )]
    fn with_contract(adapter: ClientAdapter, proxy: bool, container: bool) -> Self {
        let (mut tools, directory) = execution_fixture(proxy);
        pin_fixture_linker(&mut tools, directory.path());
        let marker = directory.path().join("marker");
        let marker = marker.to_str().unwrap().replace('\'', "'\\''");
        let sentinel = format!("#!/bin/sh\nprintf launched > '{marker}'\nexit 99\n");
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
            fs::write(tools.executable(name).unwrap(), &sentinel).unwrap();
            if let Some(path) = tools.installed_executable(name) {
                fs::write(path, &sentinel).unwrap();
            }
        }
        fs::write(tools.native_linker().unwrap(), &sentinel).unwrap();
        fs::write(directory.path().join("rustup-home/settings.toml"), "version='12'\nauto_install='disable'\ndefault_toolchain='1.94.0-x86_64-unknown-linux-gnu'\n").unwrap();
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = tempfile::tempdir_in(base).unwrap();
        let cwd = root.path().join("workspace");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir(cwd.join("src")).unwrap();
        fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname='literal'\nversion='1.0.0'\nbuild=false\n",
        )
        .unwrap();
        fs::write(cwd.join("src/main.rs"), "fn main() {}\n").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut document: toml::Value =
            toml::from_str(include_str!("../../../../memory-hooks-v6.toml.example")).unwrap();
        document["transport"]["memoryd_url"] =
            format!("http://{}/", listener.local_addr().unwrap()).into();
        document["transport"]["unauthenticated"] = true.into();
        document["transport"]
            .as_table_mut()
            .unwrap()
            .remove("api_token");
        document["state_root"] = root.path().join("state").to_str().unwrap().into();
        let binding = document["bindings"][0].as_table_mut().unwrap();
        binding.remove("common_dir");
        binding.remove("primary_worktree");
        binding.insert("kind".to_owned(), "directory".into());
        binding.insert("root".to_owned(), cwd.to_str().unwrap().into());
        binding.insert("allow_subdirectories".to_owned(), true.into());
        document["client"]["adapter"] = adapter.name().into();
        let pin = if adapter == ClientAdapter::CodexV1 {
            "codex-cli-0.158.0"
        } else {
            "claude-code-2.1.92"
        };
        document["delegation"]["client_pin"] = pin.into();
        if adapter == ClientAdapter::CodexV1 {
            document["client"]
                .as_table_mut()
                .unwrap()
                .insert("additional_context_limit".to_owned(), 0_i64.into());
            document["delegation"]["mode"] = "advisory".into();
            for field in [
                "delivery",
                "child_identity",
                "blocking_pre_tool",
                "parent_spawn",
            ] {
                document["delegation"][field] = "unverified".into();
            }
        }
        document["rust_execution"]["client_pin"] = pin.into();
        document["rust_execution"]["boot_id"] = "00000000-0000-0000-0000-000000000001".into();
        document["rust_execution"]["namespace_device"] = 9_i64.into();
        document["rust_execution"]["namespace_inode"] = 10_i64.into();
        document["rust_execution"]["root_device"] = 1_i64.into();
        document["rust_execution"]["root_inode"] = 1_i64.into();
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
            document["rust_execution"]["executables"][name] =
                tools.executable(name).unwrap().to_str().unwrap().into();
        }
        for name in ["cargo", "rustc", "rustdoc", "cargo-clippy", "clippy-driver"] {
            document["rust_execution"]["installed_executables"][name] = tools
                .installed_executable(name)
                .unwrap()
                .to_str()
                .unwrap()
                .into();
        }
        let mut environment: toml::map::Map<String, toml::Value> = tools
            .environment()
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().into()))
            .collect();
        environment.insert("RUSTUP_TOOLCHAIN".to_owned(), tools.toolchain().into());
        environment.insert(
            "CARGO_TARGET_DIR".to_owned(),
            if container {
                "/tmp/target"
            } else {
                "/disk/target"
            }
            .into(),
        );
        if container {
            environment.insert("TMPDIR".to_owned(), "/tmp/target/compiler-temp".into());
            assert!(!proxy);
            environment.insert("CARGO_HOME".to_owned(), "/tmp/target/cargo-home".into());
            document["bindings"][0]["context"]["profile"] = "woodpecker-container".into();
            let storage: toml::Value = toml::from_str(&format!("contract='isolated-woodpecker-storage-v1'\nbinding_root={:?}\nproject='example'\nboot_id='00000000-0000-0000-0000-000000000001'\nnamespace_device=9\nnamespace_inode=10\nroot_device=1\nroot_inode=1\nisolated_woodpecker=true\n[[mounts]]\nid=2\nroot='/'\nmajor=0\nminor=2\nmountpoint='/tmp'\nmountpoint_device=2\nmountpoint_inode=3\nfilesystem='tmpfs'\nprovenance='container-local'\n",cwd.to_str().unwrap())).unwrap();
            document
                .as_table_mut()
                .unwrap()
                .insert("storage_execution".to_owned(), storage);
        }
        document["rust_execution"]["environment"] = environment.into();
        document["rust_execution"].as_table_mut().unwrap().insert(
            "native_linker".to_owned(),
            toml::Value::Table(
                [
                    ("semantics".to_owned(), "rust-lld-1.94.0-linux-v1".into()),
                    (
                        "path".to_owned(),
                        tools.native_linker().unwrap().to_str().unwrap().into(),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        );
        let config = root.path().join("hooks.toml");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config)
            .unwrap();
        file.write_all(toml::to_string(&document).unwrap().as_bytes())
            .unwrap();
        let control = root.path().join("control");
        let id = Installation::initialize(&control, adapter).unwrap();
        let installation = Installation::open(&control, id, adapter).unwrap();
        installation.activate(&config).unwrap();
        let active = installation.active().unwrap();
        let binding = active.config.resolve(&cwd).unwrap();
        let pack = GuardrailPack::new(
            binding.project().to_owned(),
            binding.context().clone(),
            1,
            vec![CanonicalRule {
                project: binding.project().to_owned(),
                id: Uuid::from_u128(1),
                policy_key: Some("rust.build.storage.workstation".to_owned()),
                revision: Some(1),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: BTreeMap::from([
                    (
                        "rust.build.target_storage".to_owned(),
                        if container {
                            "container-local-tmp"
                        } else {
                            "persistent-disk"
                        }
                        .to_owned(),
                    ),
                    (
                        "rust.build.compiler_tmp_storage".to_owned(),
                        "persistent-disk".to_owned(),
                    ),
                ]),
                content: "SENTINEL_POLICY_SECRET".to_owned(),
                overrides: None,
            }],
        )
        .unwrap();
        let pack = if container {
            let mut rules = pack.mandatory;
            rules[0].values.remove("rust.build.compiler_tmp_storage");
            GuardrailPack::new(
                pack.project,
                pack.context,
                pack.resolver_schema_version,
                rules,
            )
            .unwrap()
        } else {
            pack
        };
        Self {
            tools: directory,
            root,
            installation,
            cwd,
            pack,
            children: Cell::new(0),
            listener,
        }
    }

    fn event(&self, command: &str, child: bool) -> PreToolEvent {
        // The outer callback identity/duplicate grammar is exercised by the
        // real supervised tests. This private fixture starts after validation.
        PreToolEvent {
            session_id: "session".to_owned(),
            child: child.then(|| {
                ChildIdentity::from_validated("session", &format!("child-{}", self.children.get()))
            }),
            cwd: self.cwd.clone(),
            category: ToolCategory::Shell,
            spawn: SpawnKind::None,
            execution: Some(ExecutionInput::from_value(&json!({"command":command})).unwrap()),
        }
    }

    fn prepare(&self, child: bool) {
        std::env::set_current_dir(&self.cwd).unwrap();
        let parent = self.installation.begin_session("session").unwrap();
        self.installation
            .attach_binding(&parent, &self.cwd)
            .unwrap();
        self.installation
            .complete_session(&parent, &self.cwd, self.pack.clone(), &mut Vec::new())
            .unwrap();
        if child {
            self.children.set(self.children.get() + 1);
            let identity =
                ChildIdentity::from_validated("session", &format!("child-{}", self.children.get()));
            let pending = self.installation.begin_child(&identity).unwrap();
            self.installation
                .attach_binding(&pending, &self.cwd)
                .unwrap();
            let proof = self
                .installation
                .capture_parent("session", &self.cwd)
                .unwrap();
            self.installation
                .complete_child(&identity, &pending, &proof, &mut Vec::new())
                .unwrap();
        }
    }

    fn claim(&self, child: bool) -> ClaimedCheck {
        self.prepare(child);
        if child {
            let identity =
                ChildIdentity::from_validated("session", &format!("child-{}", self.children.get()));
            self.installation
                .claim_child_check(&identity, &self.cwd)
                .unwrap()
        } else {
            self.installation.claim_check("session", &self.cwd).unwrap()
        }
    }

    fn run(
        &self,
        command: &str,
        child: bool,
        runtime: &FixtureRuntime,
    ) -> std::result::Result<(), super::Failure> {
        let claim = self.claim(child);
        let active = self.installation.active().unwrap();
        let event = self.event(command, child);
        let check = Check {
            installation: &self.installation,
            config: &active.config,
            claim: &claim,
            event: &event,
            input: b"SENTINEL_COMMAND_SECRET",
            deadline: Instant::now()
                + if runtime.late {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(10)
                },
            gate: active.config.gate().unwrap(),
            spawn: false,
        };
        let result = check.finish_using(runtime);
        if result.is_err() {
            self.installation.fail_check(&claim).unwrap();
        }
        result
    }

    fn last_audit(&self) -> Value {
        assert!(
            !self.tools.path().join("marker").exists(),
            "analyzed executable must never launch"
        );
        let counter: Value = serde_json::from_slice(
            &fs::read(self.root.path().join("control/audit-counter")).unwrap(),
        )
        .unwrap();
        let path = self.root.path().join(format!(
            "control/audit-{:08}",
            counter["records"].as_u64().unwrap()
        ));
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= 2048);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("SENTINEL") && !text.contains('/'));
        serde_json::from_str(&text).unwrap()
    }
}

// Virtual empty output tree at the registry's exact container target. CI uses
// that same path for real compiler caches; fixture observations must never
// read those unrelated artifacts. This provider exists only in test binaries.
#[derive(Default)]
struct FixtureReads {
    inaccessible: Option<PathBuf>,
}

impl FixtureReads {
    fn virtual_output(
        path: &Path,
        deadline: Instant,
    ) -> std::result::Result<bool, AnalysisFailure> {
        crate::rust_build::paths::validate(path)?;
        if Instant::now() >= deadline {
            return Err(AnalysisFailure::Deadline);
        }
        if !path.starts_with("/tmp/target") {
            return Ok(false);
        }
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(AnalysisFailure::Read);
        }
        Ok(true)
    }
}

impl ReadProvider for FixtureReads {
    fn observe(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> std::result::Result<Observation, AnalysisFailure> {
        if self.inaccessible.as_deref() == Some(path) {
            return Err(AnalysisFailure::Read);
        }
        if Self::virtual_output(path, deadline)? {
            Ok(Observation::missing([0; 32]))
        } else {
            Filesystem.observe(path, deadline)
        }
    }
    fn directory(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> std::result::Result<Observation, AnalysisFailure> {
        if Self::virtual_output(path, deadline)? {
            Ok(Observation::missing([0; 32]))
        } else {
            Filesystem.directory(path, deadline)
        }
    }
    fn output_directory(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> std::result::Result<Observation, AnalysisFailure> {
        if Self::virtual_output(path, deadline)? {
            Ok(Observation::missing([0; 32]))
        } else {
            Filesystem.output_directory(path, deadline)
        }
    }
    fn executable(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> std::result::Result<Observation, AnalysisFailure> {
        if Self::virtual_output(path, deadline)? {
            Ok(Observation::missing([0; 32]))
        } else {
            Filesystem.executable(path, deadline)
        }
    }
}

#[test]
fn fixture_output_observations_cannot_read_ci_compiler_caches() {
    let end = Instant::now() + Duration::from_secs(5);
    for path in [
        "/tmp/target",
        "/tmp/target/debug",
        "/tmp/target/cargo-home/config",
    ] {
        assert!(
            FixtureReads::default()
                .observe(Path::new(path), end)
                .unwrap()
                == Observation::missing([0; 32])
        );
        assert!(
            FixtureReads::default()
                .output_directory(Path::new(path), end)
                .unwrap()
                == Observation::missing([0; 32])
        );
    }
    assert!(!FixtureReads::virtual_output(Path::new("/tmp/target-other"), end).unwrap());
    assert!(
        FixtureReads::default()
            .observe(Path::new("/tmp/target/../outside"), end)
            .is_err()
    );
    assert!(
        FixtureReads::default()
            .observe(Path::new("/tmp/target"), Instant::now())
            .is_err()
    );
}

struct FixtureRuntime {
    reads: FixtureReads,
    volatile: Option<CandidateRole>,
    backing: storage::FixtureBacking,
    missing_locality: bool,
    calls: Cell<usize>,
    replacement: Option<(PathBuf, Vec<u8>)>,
    late: bool,
    locality_changes: bool,
    locality_calls: Cell<usize>,
}

impl Default for FixtureRuntime {
    fn default() -> Self {
        Self {
            reads: FixtureReads::default(),
            volatile: None,
            backing: storage::FixtureBacking::Disk,
            missing_locality: false,
            calls: Cell::new(0),
            replacement: None,
            late: false,
            locality_changes: false,
            locality_calls: Cell::new(0),
        }
    }
}

impl Runtime for FixtureRuntime {
    type Reader = FixtureReads;
    fn reader(&self) -> &FixtureReads {
        &self.reads
    }
    fn locality(&self, contract: &RustExecution, deadline: Instant) -> Result<()> {
        self.locality_calls.set(self.locality_calls.get() + 1);
        if self.missing_locality || (self.locality_changes && self.locality_calls.get() >= 3) {
            return Err(Error::RustExecutionUnsupported);
        }
        contract
            .recheck_executables(deadline)
            .map_err(|_| Error::RustExecutionUnsupported)
    }
    fn evaluate(
        &self,
        binding: &crate::ResolvedBinding,
        claim: &ClaimedCheck,
        candidates: &[Candidate],
        _deadline: Instant,
    ) -> Result<Report> {
        self.calls.set(self.calls.get() + 1);
        if let Some((path, contents)) = &self.replacement {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        if self.late {
            std::thread::sleep(Duration::from_secs(2));
        }
        Ok(storage::fixture_evaluate(
            binding,
            &claim.pack,
            candidates,
            self.volatile,
            self.backing,
        ))
    }
}

fn isolated(name: &str) -> bool {
    if std::env::var("MEMORY_HOOKS_RUST_GATE_FIXTURE")
        .ok()
        .as_deref()
        == Some(name)
    {
        return false;
    }
    let full_name = format!("pre_tool::rust_gate::tests::{name}");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &full_name, "--test-threads=1"])
        .env("MEMORY_HOOKS_RUST_GATE_FIXTURE", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[test]
fn every_named_action_reaches_exact_claim_storage_audit_and_neutral() {
    if isolated("every_named_action_reaches_exact_claim_storage_audit_and_neutral") {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::new(adapter);
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
        ] {
            let runtime = FixtureRuntime::default();
            let result = rig.run(command, false, &runtime);
            assert!(
                result.is_ok(),
                "{command}: {:?}",
                result.err().map(|failure| failure.reason)
            );
            assert_eq!(runtime.calls.get(), 1);
            let record = rig.last_audit();
            assert_eq!(record["version"], 3);
            assert_eq!(record["intent"], "neutral");
            assert_eq!(record["rust"]["analysis"], "complete_build");
            assert_eq!(record["rust"]["storage"]["outcome"], "pass");
            assert!(record["rust"]["candidates"].as_u64().unwrap() >= 4);
        }
        let active = rig.installation.active().unwrap();
        let linker = active
            .config
            .rust_execution()
            .unwrap()
            .native_linker()
            .unwrap()
            .display();
        for command in [
            format!(
                "rustc src/main.rs --out-dir /disk/output -C linker={linker} -C linker-flavor=ld.lld"
            ),
            format!(
                "rustc --test src/main.rs --out-dir /disk/output -C linker={linker} -C linker-flavor=ld.lld"
            ),
            "rustdoc src/main.rs -o /disk/output".to_owned(),
        ] {
            let runtime = FixtureRuntime::default();
            let result = rig.run(&command, false, &runtime);
            assert!(
                result.is_ok(),
                "{command}: {:?}",
                result.err().map(|failure| failure.reason)
            );
            assert_eq!(runtime.calls.get(), 1);
            assert_eq!(rig.last_audit()["rust"]["storage"]["outcome"], "pass");
        }
        for command in [
            "cargo --version",
            "cargo build --help",
            "rustc --help",
            "rustdoc --version",
        ] {
            let runtime = FixtureRuntime::default();
            assert!(rig.run(command, false, &runtime).is_ok());
            assert_eq!(runtime.calls.get(), 0);
            let record = rig.last_audit();
            assert_eq!(record["rust"]["analysis"], "audited_read_only");
            assert_eq!(record["rust"]["candidates"], 0);
            assert_eq!(record["rust"]["storage"], Value::Null);
        }
    }
}

#[test]
fn temp_source_unknown_and_execution_races_deny_even_with_disk_target() {
    if isolated("temp_source_unknown_and_execution_races_deny_even_with_disk_target") {
        return;
    }
    let rig = Rig::new(ClientAdapter::ClaudeV1);
    for (runtime, expected) in [
        (
            FixtureRuntime {
                volatile: Some(CandidateRole::CompilerTemporary),
                ..FixtureRuntime::default()
            },
            "rust_storage_denied",
        ),
        (
            FixtureRuntime {
                volatile: Some(CandidateRole::SourceWorktree),
                ..FixtureRuntime::default()
            },
            "rust_storage_denied",
        ),
        (
            FixtureRuntime {
                backing: storage::FixtureBacking::Unknown,
                ..FixtureRuntime::default()
            },
            "rust_storage_indeterminate",
        ),
        (
            FixtureRuntime {
                backing: storage::FixtureBacking::Race,
                ..FixtureRuntime::default()
            },
            "rust_storage_indeterminate",
        ),
        (
            FixtureRuntime {
                missing_locality: true,
                ..FixtureRuntime::default()
            },
            "rust_execution_unsupported",
        ),
    ] {
        let failure = rig.run("cargo build", true, &runtime).err().unwrap();
        assert_eq!(failure.reason.code(), expected);
        let record = rig.last_audit();
        assert_eq!(record["reason"], expected);
        assert_eq!(record["event_kind"], "child_pre_tool");
        assert!(
            rig.installation.read_snapshot("session", &rig.cwd).is_ok(),
            "child failure must leave its parent usable"
        );
    }
}

#[test]
fn unsupported_shell_and_configuration_receive_safe_analysis_evidence() {
    if isolated("unsupported_shell_and_configuration_receive_safe_analysis_evidence") {
        return;
    }
    let rig = Rig::new(ClientAdapter::CodexV1);
    for command in [
        "opaque-no-cargo-word",
        "cargo metadata",
        "cargo install remote-package",
        "cargo build $(touch marker)",
        "cargo build; touch marker",
        "cargo build --artifact-dir /opaque",
        "cargo +nightly build",
    ] {
        let runtime = FixtureRuntime::default();
        let failure = rig.run(command, false, &runtime).err().unwrap();
        assert_eq!(failure.reason.code(), "rust_analysis_indeterminate");
        assert_eq!(runtime.calls.get(), 0);
        assert_eq!(rig.last_audit()["rust"]["analysis"], "indeterminate");
    }
    assert!(!rig.cwd.join("marker").exists());
}

#[test]
fn literal_cwd_and_exact_container_outputs_have_complete_positive_cases() {
    if isolated("literal_cwd_and_exact_container_outputs_have_complete_positive_cases") {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let installed = Rig::with_contract(adapter, false, false);
        let runtime = FixtureRuntime::default();
        let result = installed.run("cd src && cargo build", false, &runtime);
        assert!(
            result.is_ok(),
            "{:?}",
            result.err().map(|failure| failure.reason)
        );
        assert_eq!(runtime.calls.get(), 1);
        assert_eq!(installed.last_audit()["rust"]["storage"]["outcome"], "pass");
        let container = Rig::with_contract(adapter, false, true);
        let runtime = FixtureRuntime::default();
        let result = container.run("cargo build", false, &runtime);
        assert!(
            result.is_ok(),
            "{:?}",
            result.err().map(|failure| failure.reason)
        );
        assert_eq!(container.last_audit()["rust"]["storage"]["outcome"], "pass");
    }
}

mod supervised;
