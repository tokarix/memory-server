//! Real event, fresh-pack, worker and supervisor protocol with private providers.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use super::{FixtureRuntime, Rig, isolated};
use crate::config::ClientAdapter;
use crate::installation::Installation;
use crate::pre_tool::{self, DenyReason};
use crate::session_start::read_event_bytes;
use crate::storage::CandidateRole;

const ENTRY: &str = "pre_tool::rust_gate::tests::supervised::supervised_worker_fixture";

#[test]
fn supervised_worker_fixture() {
    let Some(control) = std::env::var_os("MEMORY_HOOKS_FIXTURE_CONTROL") else {
        return;
    };
    let client =
        ClientAdapter::from_name(&std::env::var("MEMORY_HOOKS_FIXTURE_CLIENT").unwrap()).unwrap();
    let id = Uuid::parse_str(&std::env::var("MEMORY_HOOKS_FIXTURE_INSTALLATION").unwrap()).unwrap();
    let installation = Installation::open(&std::path::PathBuf::from(control), id, client).unwrap();
    let mode = std::env::var("MEMORY_HOOKS_FIXTURE_MODE").unwrap();
    let runtime = FixtureRuntime {
        volatile: match mode.as_str() {
            "temp" => Some(CandidateRole::CompilerTemporary),
            "source" => Some(CandidateRole::SourceWorktree),
            _ => None,
        },
        backing: match mode.as_str() {
            "unknown" => crate::storage::FixtureBacking::Unknown,
            "race" => crate::storage::FixtureBacking::Race,
            "symlink" => crate::storage::FixtureBacking::SymlinkEscape,
            "mount" => crate::storage::FixtureBacking::NestedMount,
            _ => crate::storage::FixtureBacking::Disk,
        },
        missing_locality: mode == "locality",
        locality_changes: mode == "namespace",
        replacement: match mode.as_str() {
            "config" => Some((
                std::env::current_dir().unwrap().join(".cargo/config"),
                b"[build]\ntarget-dir='/volatile/new'\n".to_vec(),
            )),
            "executable" => Some((
                installation
                    .active()
                    .unwrap()
                    .config
                    .rust_execution()
                    .unwrap()
                    .installed_executable("rustc")
                    .unwrap()
                    .to_path_buf(),
                b"REPLACED_SECRET_SENTINEL".to_vec(),
            )),
            _ => None,
        },
        ..FixtureRuntime::default()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let bytes = read_event_bytes(deadline).unwrap();
    // SAFETY: the test parent installs its dedicated protocol socket at fd 3
    // in pre_exec; this process owns it and creates exactly one File owner.
    let mut writer = unsafe { File::from_raw_fd(3) };
    let result = pre_tool::run_worker_with(&installation, &mut writer, &bytes, deadline, |check| {
        if mode == "crash" {
            std::process::exit(91);
        }
        if mode == "late" {
            thread::sleep(Duration::from_secs(3));
        }
        if mode == "supersede" {
            let pending = installation.begin_session("session").unwrap();
            installation
                .attach_binding(&pending, check.event.cwd())
                .unwrap();
            installation
                .complete_session(
                    &pending,
                    check.event.cwd(),
                    check.claim.pack.clone(),
                    &mut Vec::new(),
                )
                .unwrap();
        }
        check.finish_using(&runtime)
    });
    std::process::exit(if result.is_ok() { 0 } else { 92 });
}

fn worker(rig: &Rig, mode: &str) -> Child {
    let (read, write) = UnixStream::pair().unwrap();
    let fd = write.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", ENTRY, "--test-threads=1"])
        .current_dir(&rig.cwd)
        .env(
            "MEMORY_HOOKS_FIXTURE_CONTROL",
            rig.root.path().join("control"),
        )
        .env(
            "MEMORY_HOOKS_FIXTURE_INSTALLATION",
            rig.installation.id().to_string(),
        )
        .env(
            "MEMORY_HOOKS_FIXTURE_CLIENT",
            rig.installation.client().name(),
        )
        .env("MEMORY_HOOKS_FIXTURE_MODE", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(rig.root.path().join("worker-stderr")).unwrap(),
        ));
    // SAFETY: the child closure only invokes async-signal-safe libc calls and
    // installs the still-live socket descriptor before exec. No Rust locks.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(write);
    let descriptor: OwnedFd = read.into();
    child.stdout = Some(ChildStdout::from(descriptor));
    child
}

fn event(rig: &Rig, input: &Value, child: bool) -> Vec<u8> {
    let mut value = json!({"session_id":"session", "cwd":rig.cwd,
        "hook_event_name":"PreToolUse", "tool_name":"Bash", "tool_use_id":"call",
        "tool_input":input});
    if rig.installation.client() == ClientAdapter::CodexV1 {
        value["turn_id"] = "turn".into();
    }
    if child {
        value["agent_id"] = format!("child-{}", rig.children.get()).into();
    }
    serde_json::to_vec(&value).unwrap()
}

fn serve(rig: &Rig, pack: memory_common::guardrails::GuardrailPack) -> thread::JoinHandle<()> {
    let listener = rig.listener.try_clone().unwrap();
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fresh request expected");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let count = stream.read(&mut bytes).unwrap();
            assert!(count > 0 && request.len() < 65536);
            request.extend_from_slice(&bytes[..count]);
        }
        assert!(request.starts_with(b"GET /api/v1/projects/example/guardrails?context="));
        let body = serde_json::to_vec(&pack).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
    })
}

fn run(rig: &Rig, input: &Value, delegated: bool, mode: &str) -> (DenyReason, bool) {
    rig.prepare(delegated);
    let fresh = if mode == "policy" {
        let mut rules = rig.pack.mandatory.clone();
        rules[0].content = "changed authoritative pack".to_owned();
        memory_common::guardrails::GuardrailPack::new(
            rig.pack.project.clone(),
            rig.pack.context.clone(),
            rig.pack.resolver_schema_version,
            rules,
        )
        .unwrap()
    } else {
        rig.pack.clone()
    };
    let server = serve(rig, fresh);
    let bytes = event(rig, input, delegated);
    let child = worker(rig, mode);
    let result = crate::supervisor::fixture_supervise(
        &rig.installation,
        &bytes,
        Instant::now()
            + if mode == "late" {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(10)
            },
        child,
    );
    server.join().unwrap();
    result
}

#[test]
fn supervised_named_actions_native_permission_and_child_storage_matrix() {
    if isolated("supervised::supervised_named_actions_native_permission_and_child_storage_matrix") {
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
            "cargo --version",
            "rustdoc --help",
        ] {
            let (reason, neutral) = run(&rig, &json!({"command":command}), false, "disk");
            assert!(neutral, "{command}: {reason:?}");
            assert_eq!(rig.last_audit()["version"], 3);
            let mut output = Vec::new();
            assert_eq!(
                crate::supervisor::fixture_decision((reason, neutral), &mut output),
                0
            );
            assert!(output.is_empty(), "no permission allow directive");
            let mut permission_calls = 0;
            let mut permission = || {
                permission_calls += 1;
                false
            };
            let accepted = if output.is_empty() {
                permission()
            } else {
                true
            };
            assert!(!accepted);
            assert_eq!(permission_calls, 1);
            if adapter == ClientAdapter::ClaudeV1 {
                let (reason, neutral) = run(&rig, &json!({"command":command}), true, "disk");
                assert!(neutral, "child {command}: {reason:?}");
                assert_eq!(rig.last_audit()["event_kind"], "child_pre_tool");
            }
        }
        assert!(run(&rig, &json!({"argv":["cargo","check"]}), false, "disk").1);
        assert!(
            run(
                &rig,
                &json!({"command":"cargo +1.94.0 check"}),
                false,
                "disk"
            )
            .1
        );
        if adapter == ClientAdapter::ClaudeV1 {
            for (mode, expected) in [
                ("temp", "rust_storage_denied"),
                ("source", "rust_storage_denied"),
                ("unknown", "rust_storage_indeterminate"),
                ("race", "rust_storage_indeterminate"),
                ("symlink", "rust_storage_denied"),
                ("mount", "rust_storage_denied"),
                ("locality", "rust_execution_unsupported"),
            ] {
                let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), true, mode);
                assert!(!neutral);
                assert_eq!(
                    reason.code(),
                    expected,
                    "{mode}: {}",
                    std::fs::read_to_string(rig.root.path().join("worker-stderr")).unwrap()
                );
                assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
                assert_eq!(rig.last_audit()["event_kind"], "child_pre_tool");
            }
        }
    }
}

#[test]
fn supervised_replacements_namespace_deadline_and_worker_death_invalidate_exact_child() {
    if isolated(
        "supervised::supervised_replacements_namespace_deadline_and_worker_death_invalidate_exact_child",
    ) {
        return;
    }
    for (mode, expected) in [
        ("config", "rust_analysis_indeterminate"),
        ("executable", "rust_analysis_indeterminate"),
        ("namespace", "rust_execution_unsupported"),
        ("crash", "snapshot_invalid"),
        ("late", "snapshot_invalid"),
    ] {
        let rig = Rig::new(ClientAdapter::ClaudeV1);
        let began = Instant::now();
        let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), true, mode);
        assert!(!neutral, "{mode}");
        assert_eq!(reason.code(), expected, "{mode}");
        assert!(began.elapsed() < Duration::from_secs(5));
        if mode == "executable" {
            // A changed protected executable makes the whole contract stale,
            // but local retirement must still leave the parent's stored head.
            let hash = crate::snapshot::session_hash(&rig.installation, "session").unwrap();
            let head: Value = serde_json::from_slice(
                &std::fs::read(rig.root.path().join(format!("control/head-{hash}"))).unwrap(),
            )
            .unwrap();
            assert_eq!(head["state"], "emitted");
            let guard: Value = serde_json::from_slice(
                &std::fs::read(rig.root.path().join(format!("control/guard-{hash}"))).unwrap(),
            )
            .unwrap();
            assert_eq!(guard["state"], "idle");
        } else {
            assert!(
                rig.installation.read_snapshot("session", &rig.cwd).is_ok(),
                "{mode}"
            );
        }
        if !matches!(mode, "crash" | "late") {
            let record = rig.last_audit();
            assert_eq!(record["rust"]["analysis"], "indeterminate");
            assert_eq!(record["rust"]["storage"], Value::Null);
        }
    }
}

#[test]
fn supervised_static_contract_matrix_and_nonexecution_sentinels() {
    if isolated("supervised::supervised_static_contract_matrix_and_nonexecution_sentinels") {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::with_contract(adapter, false, false);
        let contract = rig
            .installation
            .active()
            .unwrap()
            .config
            .rust_execution()
            .unwrap()
            .clone();
        let linker = contract.native_linker().unwrap().display();
        for input in [
            json!({"command":format!("rustc src/main.rs --out-dir /disk/out -C linker={linker} -C linker-flavor=ld.lld")}),
            json!({"command":"rustdoc src/main.rs -o /disk/doc"}),
            json!({"command":"cd src && cargo build"}),
            json!({"command":"cargo build", "workdir":"src"}),
            json!({"command":"env -u CARGO_TARGET_DIR cargo build --target-dir '/disk/path with spaces'"}),
            json!({"command":"cargo build --config 'build.target-dir=\"/disk/first\"' --config 'build.target-dir=\"/disk/last\"'"}),
            json!({"command":"cargo build --release --target x86_64-unknown-linux-gnu"}),
            json!({"command":"rustup run 1.94.0 cargo check"}),
        ] {
            let (reason, neutral) = run(&rig, &input, false, "disk");
            assert!(neutral, "{input}: {reason:?}");
        }
        for input in [
            json!({"command":"cargo build $(touch marker)"}),
            json!({"command":"cargo build; touch marker"}),
            json!({"command":"cargo build", "env":{"PROFILE":"woodpecker-container"}}),
            json!({"command":"docker run cargo build"}),
            json!({"command":"cargo install remote"}),
            json!({"command":"cargo build", "workdir":"/"}),
        ] {
            let (reason, neutral) = run(&rig, &input, false, "disk");
            assert!(!neutral);
            assert!(matches!(
                reason,
                DenyReason::RustAnalysisIndeterminate | DenyReason::RustExecutionUnsupported
            ));
        }
        assert!(!rig.cwd.join("marker").exists());
        let container = Rig::with_contract(adapter, false, true);
        assert!(run(&container, &json!({"command":"cargo build"}), false, "disk").1);
        let same_paths = json!({"command":"cargo build --target-dir /tmp/target",
            "env":{"TMPDIR":"/tmp/target/compiler-temp", "CARGO_HOME":"/tmp/target/cargo-home"}});
        assert!(run(&container, &same_paths, false, "disk").1);
        let (reason, neutral) = run(&rig, &same_paths, false, "disk");
        assert!(!neutral);
        assert_eq!(reason.code(), "rust_storage_denied");
        let (reason, neutral) = run(
            &container,
            &json!({"command":"cargo build --target-dir /disk/escape"}),
            false,
            "disk",
        );
        assert!(!neutral);
        assert_eq!(reason.code(), "rust_storage_denied");
        let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), false, "temp");
        assert!(!neutral);
        assert_eq!(reason.code(), "rust_storage_denied");
    }
}

#[test]
fn supervised_environment_config_workspace_and_input_bounds() {
    if isolated("supervised::supervised_environment_config_workspace_and_input_bounds") {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::with_contract(adapter, false, false);
        let active = rig.installation.active().unwrap();
        let contract = active.config.rust_execution().unwrap();
        let mut argv = vec!["env".to_owned(), "-i".to_owned()];
        argv.extend(
            contract
                .environment()
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        argv.extend(["cargo".to_owned(), "build".to_owned()]);
        assert!(run(&rig, &json!({"argv":argv}), false, "disk").1);
        std::fs::create_dir(rig.cwd.join(".cargo")).unwrap();
        std::fs::write(
            rig.cwd.join(".cargo/config.toml"),
            "[build]\ntarget-dir='/disk/lower'\n",
        )
        .unwrap();
        std::fs::write(rig.cwd.join(".cargo/config"), "[env]\nTMPDIR={value='tmp',force=true,relative=true}\n[build]\ntarget-dir='/disk/preferred'\n").unwrap();
        assert!(
            run(
                &rig,
                &json!({"command":"cargo build --target-dir '/disk/flag'"}),
                false,
                "disk"
            )
            .1
        );
        let (reason, neutral) = run(
            &rig,
            &json!({"command":"cargo build", "env":{"TMPDIR":"/tmp/cargo-stage"}}),
            false,
            "disk",
        );
        assert!(!neutral, "safe forced compiler temp cannot hide Cargo temp");
        assert_eq!(reason.code(), "rust_storage_denied");
        std::fs::write(
            rig.cwd.join("Cargo.toml"),
            "[workspace]\nmembers=['member']\nresolver='2'\n",
        )
        .unwrap();
        std::fs::create_dir_all(rig.cwd.join("member/src")).unwrap();
        std::fs::write(
            rig.cwd.join("member/Cargo.toml"),
            "[package]\nname='member'\nversion='1.0.0'\nbuild=false\n",
        )
        .unwrap();
        std::fs::write(rig.cwd.join("member/src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        let (reason, neutral) = run(
            &rig,
            &json!({"command":"cargo check --workspace"}),
            false,
            "disk",
        );
        assert!(neutral, "workspace: {reason:?}");
        for input in [
            json!({"argv":vec!["cargo"; 513]}),
            json!({"command":format!("cargo build {}", "x".repeat(4097))}),
            json!({"command":"cargo build", "env":{"RUSTC_WRAPPER":"secret-wrapper"}}),
        ] {
            let (reason, neutral) = run(&rig, &input, false, "disk");
            assert!(!neutral);
            assert_eq!(reason.code(), "rust_analysis_indeterminate");
        }
        assert!(!rig.cwd.join("marker").exists());
    }
}
