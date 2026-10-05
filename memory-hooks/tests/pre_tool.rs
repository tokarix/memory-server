//! Managed pre-tool subprocess and fresh-fetch contract fixtures.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use memory_common::guardrails::GuardrailPack;
use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors};
use memory_hooks::{Installation, config::ClientAdapter};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

struct Rig {
    root: TempDir,
    installation: Installation,
    id: Uuid,
    cwd: PathBuf,
    listener: TcpListener,
    adapter: ClientAdapter,
}

impl Rig {
    fn new() -> Self {
        Self::new_with(ClientAdapter::CodexV1)
    }

    fn new_with(adapter: ClientAdapter) -> Self {
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = tempfile::tempdir_in(base).expect("private fixture");
        let cwd = root.path().join("workspace");
        fs::create_dir(&cwd).expect("workspace");
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let config = root.path().join("hooks.toml");
        let additional = if adapter == ClientAdapter::CodexV1 {
            "additional_context_limit = 0\n"
        } else {
            ""
        };
        let source = format!(
            "schema_version = 3\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n[transport]\nmemoryd_url = \"http://127.0.0.1:{port}/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = {:?}\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\n{additional}[gate]\ncontract = \"managed-pre-tool-v1\"\nasync_hook = false\naudit_location = \"installation-anchor-v1\"\naudit_max_records = 4096\naudit_max_bytes = 8388608\naudit_record_bytes = 2048\n",
            root.path().join("state").display().to_string(),
            cwd.display().to_string(),
            adapter.name()
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config)
            .expect("config");
        file.write_all(source.as_bytes()).expect("config write");
        let control = root.path().join("control");
        let id = Installation::initialize(&control, adapter).expect("initialize");
        let installation = Installation::open(&control, id, adapter).expect("open");
        installation.activate(&config).expect("activate");
        Self {
            root,
            installation,
            id,
            cwd,
            listener,
            adapter,
        }
    }

    fn pack(&self, content: &str) -> GuardrailPack {
        let binding = self
            .installation
            .active()
            .expect("active")
            .config
            .resolve(&self.cwd)
            .expect("binding");
        GuardrailPack::new(
            binding.project().to_owned(),
            binding.context().clone(),
            1,
            vec![CanonicalRule {
                project: "general".to_owned(),
                id: Uuid::from_u128(1),
                policy_key: Some("exact".to_owned()),
                revision: Some(1),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: BTreeMap::new(),
                content: content.to_owned(),
                overrides: None,
            }],
        )
        .expect("pack")
    }

    fn deliver(&self, pack: GuardrailPack) {
        let pending = self.installation.begin_session("session").expect("begin");
        self.installation
            .attach_binding(&pending, &self.cwd)
            .expect("binding");
        self.installation
            .complete_session(&pending, &self.cwd, pack, &mut Vec::new())
            .expect("emit");
    }

    fn event(&self, tool: &str, input: &Value) -> Vec<u8> {
        let mut value = json!({
            "session_id":"session",
            "cwd":self.cwd,
            "hook_event_name":"PreToolUse",
            "tool_name":tool,
            "tool_use_id":Uuid::new_v4().to_string(),
            "tool_input":input,
        });
        if self.adapter == ClientAdapter::CodexV1 {
            value["turn_id"] = json!("turn");
        }
        serde_json::to_vec(&value).expect("event")
    }

    fn invoke(&self, event: &[u8]) -> Output {
        self.spawn(event)
            .wait_with_output()
            .expect("supervisor output")
    }

    fn spawn(&self, event: &[u8]) -> std::process::Child {
        let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
            .args(["pre-tool", "--installation"])
            .arg(self.root.path().join("control"))
            .args([
                "--installation-id",
                &self.id.to_string(),
                "--client",
                self.adapter.name(),
            ])
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("supervisor");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(event)
            .expect("event write");
        child
    }
}

fn serve(listener: TcpListener, bodies: Vec<Vec<u8>>) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking");
        let mut count = 0;
        for body in bodies {
            let deadline = Instant::now() + Duration::from_secs(8);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request expected");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read deadline");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let length = stream.read(&mut chunk).expect("request read");
                assert!(length > 0);
                request.extend_from_slice(&chunk[..length]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(request.starts_with(b"GET /api/v1/projects/general/guardrails?context="));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("headers");
            stream.write_all(&body).expect("body");
            count += 1;
        }
        count
    })
}

fn storage_pack(rig: &Rig) -> GuardrailPack {
    let mut pack = rig.pack("SENTINEL_POLICY_SECRET");
    pack.mandatory[0].values = [(
        "rust.build.target_storage".to_owned(),
        "persistent-disk".to_owned(),
    )]
    .into();
    GuardrailPack::new(
        pack.project,
        pack.context,
        pack.resolver_schema_version,
        pack.mandatory,
    )
    .expect("storage pack")
}

#[test]
fn legacy_execution_and_unknown_tools_deny_enforced_rust_with_v3_safe_evidence() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::new_with(adapter);
        let pack = storage_pack(&rig);
        let proposals = [
            ("Bash", json!({"command":"cargo build"})),
            (
                "Bash",
                json!({"argv":["rustc", "source.rs", "--out-dir", "/output"]}),
            ),
            ("Bash", json!({"command":"ssh host cargo build"})),
            (
                "Bash",
                json!({"command":"opaque-no-rust-name SENTINEL_SECRET"}),
            ),
            (
                "Bash",
                json!({"command":format!("touch {}",rig.cwd.join("marker").display())}),
            ),
            (
                "remote_execution_tool",
                json!({"command":"cargo build", "profile":"woodpecker-container"}),
            ),
        ];
        let server = serve(
            rig.listener.try_clone().unwrap(),
            vec![serde_json::to_vec(&pack).unwrap(); proposals.len()],
        );
        for (index, (tool, input)) in proposals.iter().enumerate() {
            rig.deliver(pack.clone());
            let output = rig.invoke(&rig.event(tool, input));
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("rust_execution_unsupported"));
            assert!(output.stderr.is_empty());
            let text = fs::read_to_string(
                rig.root
                    .path()
                    .join(format!("control/audit-{:08}", index + 1)),
            )
            .unwrap();
            let record: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(record["version"], 3);
            assert_eq!(record["rust"]["analysis"], "indeterminate");
            assert_eq!(record["rust"]["contract"], 0);
            assert_eq!(record["rust"]["candidates"], 0);
            assert_eq!(record["rust"]["storage"], Value::Null);
            assert!(text.len() <= 2048);
            assert!(!text.contains("SENTINEL") && !text.contains("cargo") && !text.contains('/'));
        }
        assert_eq!(server.join().unwrap(), proposals.len());
        assert!(!rig.cwd.join("marker").exists());
    }
}

#[test]
fn enforced_rust_keeps_known_inspection_and_edits_under_generic_native_gate() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::new_with(adapter);
        let pack = storage_pack(&rig);
        rig.deliver(pack.clone());
        let (edit, input) = if adapter == ClientAdapter::CodexV1 {
            (
                "apply_patch",
                json!({"command":"SENTINEL_PATCH_BODY cargo build $(evil)"}),
            )
        } else {
            (
                "Write",
                json!({"file_path":"literal", "content":"SENTINEL_PATCH_BODY"}),
            )
        };
        let server = serve(
            rig.listener.try_clone().unwrap(),
            vec![serde_json::to_vec(&pack).unwrap(); 2],
        );
        for (tool, input) in [(edit, input), ("Read", json!({"file_path":"literal"}))] {
            let output = rig.invoke(&rig.event(tool, &input));
            assert!(
                output.status.success() && output.stdout.is_empty() && output.stderr.is_empty()
            );
        }
        assert_eq!(server.join().unwrap(), 2);
        assert!(!rig.cwd.join("literal").exists());
    }
}

#[test]
fn generic_execution_cwd_cannot_cross_binding_or_hide_behind_unsupported_fields() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::new_with(adapter);
        let pack = rig.pack("generic");
        rig.deliver(pack.clone());
        let server = serve(
            rig.listener.try_clone().unwrap(),
            vec![serde_json::to_vec(&pack).unwrap()],
        );
        let output = rig.invoke(&rig.event("Bash", &json!({"command":"true", "workdir":"/"})));
        assert_eq!(decision(&output).as_deref(), Some("snapshot_invalid"));
        assert_eq!(server.join().unwrap(), 1);
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
        let record: Value = serde_json::from_slice(
            &fs::read(rig.root.path().join("control/audit-00000001")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["intent"], "deny");
        assert_eq!(record["reason"], "snapshot_invalid");
        assert!(record["generation"].is_string() && record["attempt"].is_string());
        rig.deliver(pack);
        let output = rig.invoke(&rig.event(
            "Bash",
            &json!({"command":"true", "workdir":"/", "opaque_mode":"SENTINEL_SECRET"}),
        ));
        assert_eq!(decision(&output).as_deref(), Some("event_invalid"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SENTINEL"));
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
    }
}

fn serve_failure(listener: TcpListener, status: &str, body: &[u8]) -> thread::JoinHandle<()> {
    let status = status.to_owned();
    let body = body.to_vec();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("failure request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read deadline");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let length = stream.read(&mut chunk).expect("request read");
            assert!(length > 0);
            request.extend_from_slice(&chunk[..length]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:1/SENTINEL_REDIRECT\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .expect("response header");
        stream.write_all(&body).expect("response body");
    })
}

fn decision(output: &Output) -> Option<String> {
    if output.stdout.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).expect("decision JSON");
    Some(
        value["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("reason")
            .to_owned(),
    )
}

fn client_harness(output: &Output, permission: impl FnOnce() -> bool) -> bool {
    if !output.status.success() {
        return false;
    }
    if output.stdout.is_empty() {
        return permission();
    }
    let value: Value = serde_json::from_slice(&output.stdout).expect("client decision");
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(value["hookSpecificOutput"].get("updatedInput").is_none());
    assert!(
        value["hookSpecificOutput"]
            .get("updatedPermissions")
            .is_none()
    );
    false
}

fn paused_server(
    listener: TcpListener,
    pack: GuardrailPack,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("deadline");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let length = stream.read(&mut chunk).expect("request");
            assert!(length > 0);
            request.extend_from_slice(&chunk[..length]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        accepted_tx.send(()).expect("accepted");
        release_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("release");
        let body = serde_json::to_vec(&pack).expect("pack");
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(header.as_bytes()).is_ok() {
            let _ = stream.write_all(&body);
        }
    });
    (accepted_rx, release_tx, server)
}

#[test]
fn fresh_equal_pack_is_neutral_each_time_and_keeps_native_permission_flow() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let rig = Rig::new_with(adapter);
        let pack = rig.pack("same");
        rig.deliver(pack.clone());
        let server = serve(
            rig.listener.try_clone().expect("listener"),
            vec![serde_json::to_vec(&pack).expect("pack"); 3],
        );
        let mut native_calls = 0;
        for tool in ["Bash", "mcp__server__write", "SENTINEL_TOOL"] {
            let input = if tool == "Bash" {
                json!({"command":"SENTINEL_SECRET"})
            } else {
                json!({"payload":"SENTINEL_SECRET"})
            };
            let output = rig.invoke(&rig.event(tool, &input));
            assert!(output.status.success(), "{output:?}");
            assert!(output.stdout.is_empty(), "neutral must be empty");
            assert!(!client_harness(&output, || {
                native_calls += 1;
                false
            }));
        }
        assert_eq!(native_calls, 3);
        assert_eq!(server.join().expect("server"), 3);
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
        let audit =
            fs::read_to_string(rig.root.path().join("control/audit-00000001")).expect("audit");
        assert!(audit.contains("\"intent\":\"neutral\""));
        assert!(!audit.contains("SENTINEL_SECRET"));
        for index in 1..=3 {
            let audit =
                fs::read_to_string(rig.root.path().join(format!("control/audit-{index:08}")))
                    .expect("audit record");
            assert!(!audit.contains("SENTINEL_TOOL"));
            assert!(!audit.contains("SENTINEL_SECRET"));
        }
    }
}

#[test]
fn observed_unsupported_codex_child_makes_root_shaped_calls_ambiguous() {
    let rig = Rig::new_with(ClientAdapter::CodexV1);
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let event = json!({
        "hook_event_name": "SubagentStart",
        "session_id": "session",
        "agent_id": "child",
        "agent_type": "Explore",
        "turn_id": "turn",
        "cwd": rig.cwd,
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("delegated-start")
        .arg("--installation")
        .arg(rig.root.path().join("control"))
        .arg("--installation-id")
        .arg(rig.id.to_string())
        .arg("--client")
        .arg(ClientAdapter::CodexV1.name())
        .current_dir(&rig.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("delegated helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event.to_string().as_bytes())
        .expect("event");
    let outcome = child.wait_with_output().expect("start result");
    assert!(outcome.status.success());
    assert!(outcome.stdout.is_empty());
    for _ in 0..2 {
        let denial = rig.invoke(&rig.event("Bash", &json!({"command": "true"})));
        assert_eq!(decision(&denial).as_deref(), Some("unsupported_capability"));
        rig.deliver(pack.clone());
    }
}

#[test]
fn changed_policy_invalidates_until_fresh_delivery() {
    let rig = Rig::new();
    let old = rig.pack("old");
    rig.deliver(old);
    let changed = rig.pack("new");
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&changed).expect("pack")],
    );
    let first = rig.invoke(&rig.event("apply_patch", &json!({"command":"SENTINEL_PATCH"})));
    assert_eq!(decision(&first).as_deref(), Some("policy_changed"));
    assert!(!client_harness(&first, || panic!(
        "denial must skip fake tool"
    )));
    assert_eq!(server.join().expect("server"), 1);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
    let again = rig.invoke(&rig.event("unknown", &json!({})));
    assert!(decision(&again).is_some());
    rig.deliver(changed.clone());
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&changed).expect("pack")],
    );
    let fresh = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert!(fresh.status.success());
    assert!(fresh.stdout.is_empty(), "{fresh:?}");
    assert_eq!(server.join().expect("server"), 1);
}

#[test]
fn no_snapshot_denies_without_http() {
    let rig = Rig::new();
    let output = rig.invoke(&rig.event("Edit", &json!({"new_string":"SENTINEL"})));
    assert!(output.status.success());
    assert!(decision(&output).is_some());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SENTINEL"));
    let audit = fs::read_to_string(rig.root.path().join("control/audit-00000001")).expect("audit");
    assert!(audit.contains("\"intent\":\"deny\""));
    assert!(!audit.contains("SENTINEL"));
}

#[test]
fn malformed_event_is_denied_and_redacted_before_worker_dispatch() {
    let rig = Rig::new();
    let output = rig.invoke(br#"{"session_id":"SENTINEL_SESSION","session_id":"other"}"#);
    assert_eq!(decision(&output).as_deref(), Some("event_invalid"));
    let audit = fs::read_to_string(rig.root.path().join("control/audit-00000001")).expect("audit");
    assert!(!audit.contains("SENTINEL_SESSION"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SENTINEL_SESSION"));
}

#[test]
fn identifiable_malformed_event_retires_its_delivery() {
    let rig = Rig::new();
    rig.deliver(rig.pack("same"));
    let output =
        rig.invoke(br#"{"session_id":"session","cwd":"/","hook_event_name":"PreToolUse"}"#);
    assert_eq!(decision(&output).as_deref(), Some("event_invalid"));
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn malformed_child_event_cannot_retire_parent_delivery() {
    let rig = Rig::new();
    rig.deliver(rig.pack("same"));
    for identity in [
        br#"{"session_id":"session","agent_id":"child","cwd":"/","hook_event_name":"PreToolUse"}"#
            .as_slice(),
        br#"{"session_id":"session","agent_id":null,"cwd":"/","hook_event_name":"PreToolUse"}"#
            .as_slice(),
    ] {
        let output = rig.invoke(identity);
        assert_eq!(decision(&output).as_deref(), Some("event_invalid"));
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
    }
}

#[test]
fn internal_worker_cannot_be_registered_directly() {
    let rig = Rig::new();
    let output = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .args(["pre-tool-worker", "--installation"])
        .arg(rig.root.path().join("control"))
        .args([
            "--installation-id",
            &rig.id.to_string(),
            "--client",
            "codex-v1",
        ])
        .current_dir(&rig.cwd)
        .output()
        .expect("worker attempt");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn denial_output_failure_uses_blocking_exit_two() {
    let full = OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("full device");
    let output = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("pre-tool")
        .stdout(Stdio::from(full))
        .output()
        .expect("broken output invocation");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stderr, b"pre_tool: deny output unavailable\n");
}

#[test]
fn delivery_only_configuration_cannot_authorize_pretool() {
    let rig = Rig::new();
    let config = rig.root.path().join("hooks.toml");
    let source = fs::read_to_string(&config).expect("config");
    let (prefix, _) = source.split_once("[gate]").expect("gate section");
    fs::write(
        &config,
        prefix.replace("schema_version = 3", "schema_version = 2"),
    )
    .expect("delivery config");
    rig.installation.activate(&config).expect("activate v2");
    let output = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&output).as_deref(), Some("gate_contract_required"));
}

#[test]
fn audit_capacity_exhaustion_denies_and_invalidates() {
    let rig = Rig::new();
    let config = rig.root.path().join("hooks.toml");
    let source = fs::read_to_string(&config).expect("config");
    fs::write(
        &config,
        source.replace("audit_max_records = 4096", "audit_max_records = 1"),
    )
    .expect("limited config");
    rig.installation.activate(&config).expect("activate limits");
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&pack).expect("pack"); 2],
    );
    let first = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert!(first.stdout.is_empty(), "{first:?}");
    let second = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&second).as_deref(), Some("audit_failed"));
    assert_eq!(server.join().expect("server"), 2);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn malformed_fresh_reply_retires_prior_success() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&pack).expect("pack"), b"{}".to_vec()],
    );
    let first = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert!(first.stdout.is_empty(), "{first:?}");
    let second = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&second).as_deref(), Some("guardrails_unavailable"));
    assert_eq!(server.join().expect("server"), 2);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn upstream_refusal_and_redirect_are_redacted_and_retire_delivery() {
    for status in ["401 Unauthorized", "302 Found"] {
        let rig = Rig::new();
        let config = rig.root.path().join("hooks.toml");
        let source = fs::read_to_string(&config).expect("config");
        fs::write(
            &config,
            source.replace(
                "unauthenticated = true",
                "unauthenticated = false\napi_token = \"SENTINEL_TOKEN\"",
            ),
        )
        .expect("credential config");
        rig.installation
            .activate(&config)
            .expect("activate credential");
        rig.deliver(rig.pack("same"));
        let server = serve_failure(
            rig.listener.try_clone().expect("listener"),
            status,
            b"SENTINEL_UPSTREAM\x1b[31m",
        );
        let output = rig.invoke(&rig.event("Bash", &json!({"command":"SENTINEL_COMMAND"})));
        assert_eq!(decision(&output).as_deref(), Some("guardrails_unavailable"));
        server.join().expect("failure server");
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
        let audit =
            fs::read_to_string(rig.root.path().join("control/audit-00000001")).expect("audit");
        for secret in [
            "SENTINEL_TOKEN",
            "SENTINEL_UPSTREAM",
            "SENTINEL_COMMAND",
            "SENTINEL_REDIRECT",
        ] {
            assert!(!audit.contains(secret));
            assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        }
    }
}

#[test]
fn same_digest_with_wrong_scope_is_not_fresh_authority() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let mut wrong = serde_json::to_value(&pack).expect("pack value");
    wrong["project"] = json!("different");
    assert_eq!(wrong["digest"], json!(pack.digest));
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&wrong).expect("wrong pack")],
    );
    let output = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&output).as_deref(), Some("scope_mismatch"));
    assert_eq!(server.join().expect("server"), 1);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn competing_event_invalidates_the_inflight_generation() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let (accepted_rx, release_tx, server) =
        paused_server(rig.listener.try_clone().expect("listener"), pack);
    let first = rig.spawn(&rig.event("Bash", &json!({"command":"true"})));
    accepted_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("fetch started");
    let second = rig.invoke(&rig.event("Edit", &json!({"new_string":"other"})));
    assert!(decision(&second).is_some(), "{second:?}");
    release_tx.send(()).expect("release");
    let first = first.wait_with_output().expect("first output");
    assert!(decision(&first).is_some(), "{first:?}");
    server.join().expect("server");
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn fresh_delivery_supersedes_a_delayed_check_without_late_poisoning() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let (accepted_rx, release_tx, server) =
        paused_server(rig.listener.try_clone().expect("listener"), pack.clone());
    let first = rig.spawn(&rig.event("Bash", &json!({"command":"true"})));
    accepted_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("fetch started");
    rig.deliver(pack);
    release_tx.send(()).expect("release");
    let first = first.wait_with_output().expect("first output");
    assert!(decision(&first).is_some(), "{first:?}");
    server.join().expect("server");
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
}

#[test]
fn killed_worker_cannot_turn_partial_claim_into_neutral() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let (accepted_rx, release_tx, server) =
        paused_server(rig.listener.try_clone().expect("listener"), pack);
    let first = rig.spawn(&rig.event("Bash", &json!({"command":"true"})));
    accepted_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("worker claimed and fetched");
    let children = fs::read_to_string(format!("/proc/{}/task/{}/children", first.id(), first.id()))
        .expect("supervisor children");
    let worker: i32 = children
        .split_whitespace()
        .next()
        .expect("worker pid")
        .parse()
        .expect("worker pid parse");
    // SAFETY: the pid was read from this fixture supervisor's child list.
    assert_eq!(unsafe { libc::kill(worker, libc::SIGKILL) }, 0);
    release_tx.send(()).expect("release server");
    let output = first.wait_with_output().expect("supervisor output");
    assert_eq!(decision(&output).as_deref(), Some("snapshot_invalid"));
    server.join().expect("server");
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn corrupt_guard_is_retired_before_any_fetch() {
    let rig = Rig::new();
    rig.deliver(rig.pack("same"));
    let guard = fs::read_dir(rig.root.path().join("control"))
        .expect("control")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("guard-") && !name.starts_with("guard-journal-")
                })
        })
        .expect("guard record");
    fs::write(guard, b"corrupt").expect("corrupt guard");
    let output = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&output).as_deref(), Some("snapshot_invalid"));
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn corrupt_payload_is_retired_before_any_fetch() {
    let rig = Rig::new();
    rig.deliver(rig.pack("same"));
    let payload = fs::read_dir(rig.root.path().join("state"))
        .expect("state")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("payload-"))
        })
        .expect("payload");
    fs::write(payload, b"corrupt").expect("corrupt payload");
    let output = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&output).as_deref(), Some("snapshot_invalid"));
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}

#[test]
fn corrupt_audit_accounting_denies_and_invalidates() {
    let rig = Rig::new();
    let pack = rig.pack("same");
    rig.deliver(pack.clone());
    let server = serve(
        rig.listener.try_clone().expect("listener"),
        vec![serde_json::to_vec(&pack).expect("pack"); 2],
    );
    let first = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert!(first.stdout.is_empty(), "{first:?}");
    fs::write(rig.root.path().join("control/audit-counter"), b"corrupt")
        .expect("corrupt audit counter");
    let second = rig.invoke(&rig.event("Bash", &json!({"command":"true"})));
    assert_eq!(decision(&second).as_deref(), Some("audit_failed"));
    assert_eq!(server.join().expect("server"), 2);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
}
