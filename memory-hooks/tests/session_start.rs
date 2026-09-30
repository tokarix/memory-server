//! Subprocess contract for fresh client-consumed mandatory startup output.
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use memory_common::guardrails::{
    GuardrailPack, MAX_CANONICAL_BYTES, MAX_POLICIES, MAX_PUBLICATION_BYTES,
};
use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors};
use memory_hooks::{Installation, config::ClientAdapter};
use tempfile::TempDir;
use uuid::Uuid;

fn private_fixture() -> TempDir {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    tempfile::tempdir_in(base).expect("private fixture")
}

fn configured(
    root: &Path,
    adapter: ClientAdapter,
    port: u16,
) -> (Installation, Uuid, std::path::PathBuf) {
    let cwd = root.join("workspace");
    fs::create_dir(&cwd).expect("workspace");
    let control = root.join("control");
    let config = root.join("config.toml");
    let additional = if adapter == ClientAdapter::CodexV1 {
        "additional_context_limit = 0\n"
    } else {
        ""
    };
    let source = format!(
        "schema_version = 2\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n[transport]\nmemoryd_url = \"http://127.0.0.1:{port}/\"\napi_token = \"fixture-token\"\nunauthenticated = false\ncredential_revision = \"initial\"\n[client]\nadapter = {:?}\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\n{additional}",
        root.join("snapshots").display().to_string(),
        cwd.display().to_string(),
        adapter.name()
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)
        .expect("config");
    file.write_all(source.as_bytes()).expect("write config");
    let id = Installation::initialize(&control, adapter).expect("initialize");
    let installation = Installation::open(&control, id, adapter).expect("open");
    installation.activate(&config).expect("activate");
    (installation, id, cwd)
}

fn pack(installation: &Installation, cwd: &Path) -> GuardrailPack {
    let binding = installation
        .active()
        .expect("active")
        .config
        .resolve(cwd)
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
            content: "Rüle \"exact\"\r\n\t🦀 e\u{301}".to_owned(),
            overrides: None,
        }],
    )
    .expect("pack")
}

fn invalid_pack_responses(good: &GuardrailPack) -> Vec<(&'static str, &'static str, Vec<u8>)> {
    let mut wrong_project = good.clone();
    "other".clone_into(&mut wrong_project.project);
    let mut wrong_context = good.clone();
    wrong_context.context.profile = Some("container".to_owned());
    let mut wrong_digest = good.clone();
    wrong_digest.digest = format!("sha256:{}", "0".repeat(64));
    let mut wrong_version = good.clone();
    wrong_version.schema_version = 2;
    let mut empty = good.clone();
    empty.mandatory.clear();
    let mut duplicate = good.clone();
    duplicate.mandatory.push(good.mandatory[0].clone());
    let mut unordered = good.clone();
    let mut second_rule = good.mandatory[0].clone();
    second_rule.id = Uuid::from_u128(2);
    second_rule.policy_key = Some("zzz".to_owned());
    unordered.mandatory.insert(0, second_rule);
    let mut invalid_selector = serde_json::to_value(good).expect("pack value");
    invalid_selector["mandatory"][0]["selectors"]["profile"] = serde_json::json!([""]);
    let mut missing_selector = serde_json::to_value(good).expect("pack value");
    missing_selector["mandatory"][0]
        .as_object_mut()
        .expect("rule object")
        .remove("selectors");
    let mut missing_key = good.clone();
    missing_key.mandatory[0].policy_key = None;
    let mut missing_revision = good.clone();
    missing_revision.mandatory[0].revision = None;
    let mut empty_content = good.clone();
    empty_content.mandatory[0].content.clear();
    vec![
        (
            "wrong_project",
            "200 OK",
            serde_json::to_vec(&wrong_project).expect("pack"),
        ),
        (
            "wrong_context",
            "200 OK",
            serde_json::to_vec(&wrong_context).expect("pack"),
        ),
        (
            "wrong_digest",
            "200 OK",
            serde_json::to_vec(&wrong_digest).expect("pack"),
        ),
        (
            "wrong_version",
            "200 OK",
            serde_json::to_vec(&wrong_version).expect("pack"),
        ),
        ("empty", "200 OK", serde_json::to_vec(&empty).expect("pack")),
        (
            "duplicate",
            "200 OK",
            serde_json::to_vec(&duplicate).expect("pack"),
        ),
        (
            "unordered",
            "200 OK",
            serde_json::to_vec(&unordered).expect("pack"),
        ),
        (
            "invalid_selector",
            "200 OK",
            serde_json::to_vec(&invalid_selector).expect("pack"),
        ),
        (
            "missing_selector",
            "200 OK",
            serde_json::to_vec(&missing_selector).expect("pack"),
        ),
        (
            "missing_key",
            "200 OK",
            serde_json::to_vec(&missing_key).expect("pack"),
        ),
        (
            "missing_revision",
            "200 OK",
            serde_json::to_vec(&missing_revision).expect("pack"),
        ),
        (
            "empty_content",
            "200 OK",
            serde_json::to_vec(&empty_content).expect("pack"),
        ),
        ("malformed_json", "200 OK", b"{".to_vec()),
        ("conflict", "409 Conflict", b"{}".to_vec()),
        ("old_daemon", "404 Not Found", b"{}".to_vec()),
        ("unauthorized", "401 Unauthorized", b"{}".to_vec()),
        ("forbidden", "403 Forbidden", b"{}".to_vec()),
        ("server_error", "500 Internal Server Error", b"{}".to_vec()),
    ]
}

fn response(listener: TcpListener, body: Vec<u8>, status: &str) -> thread::JoinHandle<String> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut wire = header.into_bytes();
    wire.extend(body);
    response_wire(listener, wire)
}

fn response_wire(listener: TcpListener, wire: Vec<u8>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected HTTP request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("HTTP accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).expect("request read");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        // An oversized declared body may be rejected immediately after its
        // headers. A peer close while sending the body is expected then.
        if let Err(error) = stream.write_all(&wire) {
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ));
        }
        String::from_utf8(request).expect("request UTF-8")
    })
}

fn stalled_response(
    listener: TcpListener,
    prefix: Vec<u8>,
    release: mpsc::Receiver<()>,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected stalled HTTP request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("HTTP accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).expect("request read");
            assert!(count > 0, "complete request");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream.write_all(&prefix).expect("partial response");
        release
            .recv_timeout(Duration::from_secs(8))
            .expect("release stalled peer");
        String::from_utf8(request).expect("request UTF-8")
    })
}

fn reset_response(listener: TcpListener) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected reset request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("HTTP accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).expect("request read");
            assert!(count > 0, "complete request");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: the descriptor and linger object remain valid for this call.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&raw const linger).cast(),
                libc::socklen_t::try_from(std::mem::size_of::<libc::linger>())
                    .expect("linger size"),
            )
        };
        assert_eq!(result, 0, "configure TCP reset");
        drop(stream);
        String::from_utf8(request).expect("request UTF-8")
    })
}

fn invoke(id: Uuid, cwd: &Path, adapter: ClientAdapter, source: &str) -> std::process::Output {
    invoke_session(id, cwd, adapter, source, "session")
}

fn invoke_session(
    id: Uuid,
    cwd: &Path,
    adapter: ClientAdapter,
    source: &str,
    session: &str,
) -> std::process::Output {
    let event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": session,
        "cwd": cwd,
        "source": source,
        "agent_type": "custom-top-level"
    });
    invoke_raw(id, cwd, adapter, event.to_string().as_bytes())
}

#[test]
fn concurrent_different_sessions_keep_independent_public_authority() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for second_succeeds in [false, true] {
            let root = private_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("address").port();
            let (installation, id, cwd) = configured(root.path(), adapter, port);
            let expected = pack(&installation, &cwd);
            let body = serde_json::to_vec(&expected).expect("pack JSON");
            let (arrived_tx, arrived_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let delayed = delayed_response(
                listener.try_clone().expect("listener clone"),
                body.clone(),
                arrived_tx,
                release_rx,
            );
            let first_cwd = cwd.clone();
            let first = thread::spawn(move || {
                invoke_session(id, &first_cwd, adapter, "startup", "session-a")
            });
            arrived_rx
                .recv_timeout(Duration::from_secs(8))
                .expect("A reached HTTP");
            assert!(installation.read_snapshot("session-a", &cwd).is_err());
            let second = response(
                listener.try_clone().expect("listener clone"),
                if second_succeeds {
                    body
                } else {
                    b"unavailable".to_vec()
                },
                if second_succeeds {
                    "200 OK"
                } else {
                    "500 Internal Server Error"
                },
            );
            let b = invoke_session(id, &cwd, adapter, "startup", "session-b");
            assert_eq!(b.status.success(), second_succeeds);
            assert_eq!(b.stdout.is_empty(), !second_succeeds);
            assert_eq!(second.join().expect("B request").matches("GET ").count(), 1);
            assert_eq!(
                installation.read_snapshot("session-b", &cwd).is_ok(),
                second_succeeds
            );
            release_tx.send(()).expect("release A");
            assert_eq!(
                delayed.join().expect("A request").matches("GET ").count(),
                1
            );
            let a = first.join().expect("A helper");
            assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
            assert!(a.stderr.is_empty());
            assert_eq!(
                installation
                    .read_snapshot("session-a", &cwd)
                    .expect("A remains valid")
                    .pack,
                expected
            );
            assert_eq!(
                installation.read_snapshot("session-b", &cwd).is_ok(),
                second_succeeds
            );
        }
    }
}

fn invoke_raw(id: Uuid, cwd: &Path, adapter: ClientAdapter, event: &[u8]) -> std::process::Output {
    invoke_raw_with_env(id, cwd, adapter, event, &[])
}

fn invoke_delegated_raw(
    id: Uuid,
    cwd: &Path,
    adapter: ClientAdapter,
    event: &[u8],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("delegated-start")
        .arg("--installation")
        .arg(cwd.parent().expect("fixture root").join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(adapter.name())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn delegated helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event)
        .expect("event write");
    child.wait_with_output().expect("helper output")
}

fn invoke_pre_tool_raw(id: Uuid, cwd: &Path, event: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("pre-tool")
        .arg("--installation")
        .arg(cwd.parent().expect("fixture root").join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(ClientAdapter::ClaudeV1.name())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pre-tool helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event)
        .expect("event write");
    child.wait_with_output().expect("pre-tool output")
}

fn activate_enforced_claude(installation: &Installation, fixture: &Path) {
    let config = fixture.join("config.toml");
    let mut source = fs::read_to_string(&config).expect("config");
    source = source.replacen("schema_version = 2", "schema_version = 4", 1);
    source.push_str("[gate]\ncontract = \"managed-pre-tool-v1\"\nasync_hook = false\naudit_location = \"installation-anchor-v1\"\naudit_max_records = 4096\naudit_max_bytes = 8388608\naudit_record_bytes = 2048\n[delegation]\ncontract = \"managed-delegation-v1\"\nclient_pin = \"claude-code-2.1.92\"\nmode = \"enforced\"\nmax_depth = 1\nsynchronous_start = true\nsynchronous_pre_tool = true\ncatch_all_pre_tool = true\ndelivery = \"subagent-start-context\"\nchild_identity = \"agent-id-on-pre-tool\"\nblocking_pre_tool = \"pre-tool-deny\"\nparent_spawn = \"Agent\"\nrepeat_start = \"new-child-required\"\ncross_cwd = \"same-cwd-only\"\n");
    fs::write(&config, source).expect("v4 config");
    installation.activate(&config).expect("activate v4");
}

fn direct_child_start(cwd: &Path, agent_id: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": agent_id,
        "agent_type": "SENTINEL_PRIVATE_AGENT_TYPE_89",
        "cwd": cwd,
    })
}

fn direct_child_tool(cwd: &Path, agent_id: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "parent",
        "agent_id": agent_id,
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_use_id": Uuid::new_v4().to_string(),
        "tool_input": {"command": "SENTINEL_PRIVATE_COMMAND_89"},
    })
}

fn publish_v4_parent(
    installation: &Installation,
    id: Uuid,
    cwd: &Path,
    listener: &TcpListener,
) -> Vec<u8> {
    let body = serde_json::to_vec(&pack(installation, cwd)).expect("pack JSON");
    let reply = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let output = invoke_session(id, cwd, ClientAdapter::ClaudeV1, "startup", "parent");
    assert!(output.status.success(), "{:?}", output.stderr);
    reply.join().expect("root response");
    body
}

#[test]
fn managed_v4_child_publishes_exact_context_once_from_a_live_parent() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let expected = pack(&installation, &cwd);
    let body = serde_json::to_vec(&expected).expect("pack JSON");
    let root_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let root_output = invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent");
    assert!(root_output.status.success(), "{:?}", root_output.stderr);
    root_response.join().expect("root request");
    let child_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let event = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "SENTINEL_PRIVATE_AGENT_TYPE_89",
        "transcript_path": "SENTINEL_PRIVATE_TRANSCRIPT_89",
        "prompt": "SENTINEL_PRIVATE_PROMPT_89",
        "cwd": cwd,
    });
    let child = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        event.to_string().as_bytes(),
    );
    assert!(child.status.success(), "{:?}", child.stderr);
    assert!(child.stderr.is_empty());
    child_response.join().expect("child request");
    let published: serde_json::Value = serde_json::from_slice(&child.stdout).expect("output");
    assert_eq!(
        published["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
    assert_eq!(
        published["hookSpecificOutput"]["additionalContext"],
        expected.publication().expect("publication")
    );
    let child_tool = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "parent",
        "agent_id": "child",
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_use_id": Uuid::new_v4().to_string(),
        "tool_input": {"command": "SENTINEL_PRIVATE_COMMAND_89"},
    });
    let tool_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let guarded = invoke_pre_tool_raw(id, &cwd, child_tool.to_string().as_bytes());
    assert!(guarded.status.success(), "{:?}", guarded.stderr);
    assert!(guarded.stdout.is_empty(), "child check must be neutral");
    tool_response.join().expect("tool request");
    let mut missing = child_tool.clone();
    missing["agent_id"] = serde_json::json!("never-started");
    let missing_output = invoke_pre_tool_raw(id, &cwd, missing.to_string().as_bytes());
    let missing_deny: serde_json::Value =
        serde_json::from_slice(&missing_output.stdout).expect("deny");
    assert_eq!(
        missing_deny["hookSpecificOutput"]["permissionDecisionReason"],
        "missing_child_linkage"
    );
    check_sibling_and_spawn(
        &installation,
        id,
        &cwd,
        &listener,
        body,
        &event,
        &child_tool,
    );
    for store in [root.path().join("control"), root.path().join("snapshots")] {
        for entry in fs::read_dir(store).expect("private store") {
            let path = entry.expect("entry").path();
            if !path.is_file() {
                continue;
            }
            let record = fs::read(path).expect("private record");
            assert!(
                !record
                    .windows(b"SENTINEL_PRIVATE".len())
                    .any(|bytes| bytes == b"SENTINEL_PRIVATE")
            );
        }
    }
}

fn check_sibling_and_spawn(
    installation: &Installation,
    id: Uuid,
    cwd: &Path,
    listener: &TcpListener,
    body: Vec<u8>,
    event: &serde_json::Value,
    child_tool: &serde_json::Value,
) {
    let mut sibling_event = event.clone();
    sibling_event["agent_id"] = serde_json::json!("sibling");
    let sibling_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let sibling = invoke_delegated_raw(
        id,
        cwd,
        ClientAdapter::ClaudeV1,
        sibling_event.to_string().as_bytes(),
    );
    assert!(sibling.status.success());
    sibling_response.join().expect("sibling request");
    let repeated = invoke_delegated_raw(
        id,
        cwd,
        ClientAdapter::ClaudeV1,
        event.to_string().as_bytes(),
    );
    assert!(!repeated.status.success());
    assert!(repeated.stdout.is_empty());
    let stale = invoke_pre_tool_raw(id, cwd, child_tool.to_string().as_bytes());
    assert!(!stale.stdout.is_empty());
    let mut sibling_tool = child_tool.clone();
    sibling_tool["agent_id"] = serde_json::json!("sibling");
    let sibling_check = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let sibling_guarded = invoke_pre_tool_raw(id, cwd, sibling_tool.to_string().as_bytes());
    assert!(sibling_guarded.stdout.is_empty());
    sibling_check.join().expect("sibling check");
    let mut nested_spawn = sibling_tool.clone();
    nested_spawn["tool_name"] = serde_json::json!("Agent");
    nested_spawn["tool_input"] = serde_json::json!({});
    let nested = invoke_pre_tool_raw(id, cwd, nested_spawn.to_string().as_bytes());
    let nested_deny: serde_json::Value = serde_json::from_slice(&nested.stdout).expect("deny");
    assert_eq!(
        nested_deny["hookSpecificOutput"]["permissionDecisionReason"],
        "unsupported_capability"
    );
    let mut root_spawn = nested_spawn.clone();
    root_spawn
        .as_object_mut()
        .expect("object")
        .remove("agent_id");
    let spawn_check = response(listener.try_clone().expect("listener"), body, "200 OK");
    let parent_spawn = invoke_pre_tool_raw(id, cwd, root_spawn.to_string().as_bytes());
    assert!(parent_spawn.stdout.is_empty(), "covered parent spawn");
    spawn_check.join().expect("parent spawn check");
    root_spawn["tool_name"] = serde_json::json!("Task");
    let unsupported = invoke_pre_tool_raw(id, cwd, root_spawn.to_string().as_bytes());
    let unsupported_deny: serde_json::Value =
        serde_json::from_slice(&unsupported.stdout).expect("deny");
    assert_eq!(
        unsupported_deny["hookSpecificOutput"]["permissionDecisionReason"],
        "unsupported_capability"
    );
    sibling_tool["tool_input"] = serde_json::Value::Null;
    let malformed = invoke_pre_tool_raw(id, cwd, sibling_tool.to_string().as_bytes());
    assert!(!malformed.stdout.is_empty());
    sibling_tool["tool_input"] = serde_json::json!({"command": "true"});
    let retired = invoke_pre_tool_raw(id, cwd, sibling_tool.to_string().as_bytes());
    assert!(!retired.stdout.is_empty());
    assert!(installation.read_snapshot("parent", cwd).is_ok());
}

#[test]
fn parent_refresh_during_child_check_cannot_return_neutral() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let expected = pack(&installation, &cwd);
    let body = serde_json::to_vec(&expected).expect("pack JSON");
    let root_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    root_response.join().expect("root request");
    let child_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    assert!(
        invoke_delegated_raw(
            id,
            &cwd,
            ClientAdapter::ClaudeV1,
            start.to_string().as_bytes()
        )
        .status
        .success()
    );
    child_response.join().expect("child request");
    let tool = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "parent",
        "agent_id": "child",
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_use_id": Uuid::new_v4().to_string(),
        "tool_input": {"command": "true"},
    });
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let delayed = delayed_response(
        listener.try_clone().expect("listener"),
        body.clone(),
        arrived_tx,
        release_rx,
    );
    let child_cwd = cwd.clone();
    let child_tool =
        thread::spawn(move || invoke_pre_tool_raw(id, &child_cwd, tool.to_string().as_bytes()));
    arrived_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("child fetch arrived");
    let refresh_response = response(listener, body, "200 OK");
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "resume", "parent")
            .status
            .success()
    );
    refresh_response.join().expect("refresh request");
    release_tx.send(()).expect("release child fetch");
    delayed.join().expect("old child fetch");
    let outcome = child_tool.join().expect("child check");
    assert!(!outcome.stdout.is_empty(), "stale child must deny");
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn child_publication_rejects_context_that_the_client_would_spill() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let mut large = pack(&installation, &cwd);
    large.mandatory[0].content = "x".repeat(11_000);
    let large = GuardrailPack::new(
        large.project,
        large.context,
        large.resolver_schema_version,
        large.mandatory,
    )
    .expect("shared-valid large pack");
    let body = serde_json::to_vec(&large).expect("pack JSON");
    let root_response = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    root_response.join().expect("root request");
    let child_response = response(listener, body, "200 OK");
    let start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    let output = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        start.to_string().as_bytes(),
    );
    child_response.join().expect("child request");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn absent_parent_and_different_child_cwd_never_publish() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let mut start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "absent-parent-child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    let absent = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        start.to_string().as_bytes(),
    );
    assert!(!absent.status.success());
    assert!(absent.stdout.is_empty());
    let expected = pack(&installation, &cwd);
    let response = response(
        listener,
        serde_json::to_vec(&expected).expect("pack JSON"),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    response.join().expect("root fetch");
    let other = root.path().join("other-project");
    fs::create_dir(&other).expect("other cwd");
    start["agent_id"] = serde_json::json!("other-child");
    start["cwd"] = serde_json::json!(other);
    let changed = invoke_delegated_raw(
        id,
        &other,
        ClientAdapter::ClaudeV1,
        start.to_string().as_bytes(),
    );
    assert!(!changed.status.success());
    assert!(changed.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn changed_parent_pack_blocks_child_and_retires_only_captured_parent() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let initial = pack(&installation, &cwd);
    let first = response(
        listener.try_clone().expect("listener"),
        serde_json::to_vec(&initial).expect("pack"),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    first.join().expect("root fetch");
    let changed = pack(&installation, &cwd);
    let mut rules = changed.mandatory;
    rules[0].content = "changed".to_owned();
    let changed = GuardrailPack::new(changed.project, changed.context, 1, rules).expect("pack");
    let second = response(
        listener,
        serde_json::to_vec(&changed).expect("pack"),
        "200 OK",
    );
    let start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    let output = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        start.to_string().as_bytes(),
    );
    second.join().expect("child fetch");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_err());
}

#[test]
fn child_audit_failure_cannot_commit_usable_publication() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let expected = pack(&installation, &cwd);
    let body = serde_json::to_vec(&expected).expect("pack JSON");
    let first = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    first.join().expect("root fetch");
    let mut audit = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.path().join("control/audit-counter"))
        .expect("audit counter");
    audit.write_all(b"{").expect("corrupt counter");
    let second = response(listener, body, "200 OK");
    let start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    let output = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        start.to_string().as_bytes(),
    );
    second.join().expect("child fetch");
    assert!(!output.status.success());
    let tool = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "parent",
        "agent_id": "child",
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_use_id": Uuid::new_v4().to_string(),
        "tool_input": {"command": "true"},
    });
    let denied = invoke_pre_tool_raw(id, &cwd, tool.to_string().as_bytes());
    assert!(!denied.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn child_output_write_failure_leaves_no_usable_generation() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let expected = pack(&installation, &cwd);
    let body = serde_json::to_vec(&expected).expect("pack JSON");
    let first = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "startup", "parent")
            .status
            .success()
    );
    first.join().expect("root fetch");
    let second = response(listener, body, "200 OK");
    let start = serde_json::json!({
        "hook_event_name": "SubagentStart",
        "session_id": "parent",
        "agent_id": "child",
        "agent_type": "Explore",
        "cwd": cwd,
    });
    let full = OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("full sink");
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("delegated-start")
        .arg("--installation")
        .arg(root.path().join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(ClientAdapter::ClaudeV1.name())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .spawn()
        .expect("child helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(start.to_string().as_bytes())
        .expect("start event");
    let result = child.wait_with_output().expect("child result");
    second.join().expect("child fetch");
    assert!(!result.status.success());
    let tool = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "parent",
        "agent_id": "child",
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_use_id": Uuid::new_v4().to_string(),
        "tool_input": {"command": "true"},
    });
    assert!(
        !invoke_pre_tool_raw(id, &cwd, tool.to_string().as_bytes())
            .stdout
            .is_empty()
    );
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn child_start_requires_an_emitted_idle_parent_before_any_fetch() {
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("fixture output failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    for state in ["missing", "pending", "prepared", "invalid"] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
        activate_enforced_claude(&installation, root.path());
        if state != "missing" {
            let pending = installation
                .begin_session("parent")
                .expect("pending parent");
            if state == "prepared" {
                installation
                    .attach_binding(&pending, &cwd)
                    .expect("parent binding");
                assert!(
                    installation
                        .complete_session(
                            &pending,
                            &cwd,
                            pack(&installation, &cwd),
                            &mut FailingWriter
                        )
                        .is_err()
                );
            } else if state == "invalid" {
                let reply = response(
                    listener.try_clone().expect("listener"),
                    b"SENTINEL_PRIVATE_UPSTREAM_BODY_89".to_vec(),
                    "500 Internal Server Error",
                );
                let failure = invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "resume", "parent");
                assert!(!failure.status.success());
                reply.join().expect("failed parent fetch");
            }
        }
        let output = invoke_delegated_raw(
            id,
            &cwd,
            ClientAdapter::ClaudeV1,
            direct_child_start(&cwd, state).to_string().as_bytes(),
        );
        assert!(!output.status.success(), "{state}");
        assert!(output.stdout.is_empty(), "{state}");
        assert!(
            !invoke_pre_tool_raw(
                id,
                &cwd,
                direct_child_tool(&cwd, state).to_string().as_bytes()
            )
            .stdout
            .is_empty(),
            "{state} child mutation must deny"
        );
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        assert!(listener.accept().is_err(), "{state} reached daemon");
    }
}

#[test]
fn malformed_fresh_parent_reply_prevents_child_publication() {
    for variant in [
        "invalid_selector",
        "missing_selector",
        "wrong_project",
        "wrong_context",
    ] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
        activate_enforced_claude(&installation, root.path());
        let body = publish_v4_parent(&installation, id, &cwd, &listener);
        let malformed = invalid_pack_responses(&pack(&installation, &cwd))
            .into_iter()
            .find(|(name, _, _)| *name == variant)
            .expect("variant");
        let reply = response(listener, malformed.2, malformed.1);
        let output = invoke_delegated_raw(
            id,
            &cwd,
            ClientAdapter::ClaudeV1,
            direct_child_start(&cwd, variant).to_string().as_bytes(),
        );
        reply.join().expect("child fetch");
        assert!(!output.status.success(), "{variant}");
        assert!(output.stdout.is_empty(), "{variant}");
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert!(!diagnostics.contains("SENTINEL_PRIVATE"));
        assert!(installation.read_snapshot("parent", &cwd).is_err());
        assert!(
            !invoke_pre_tool_raw(
                id,
                &cwd,
                direct_child_tool(&cwd, variant).to_string().as_bytes()
            )
            .stdout
            .is_empty(),
            "{variant} child mutation must deny"
        );
        assert!(!body.is_empty());
    }
}

#[test]
fn child_daemon_outage_retires_only_the_captured_parent_generation() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    publish_v4_parent(&installation, id, &cwd, &listener);
    let failed = response(
        listener,
        b"SENTINEL_PRIVATE_UPSTREAM_BODY_89".to_vec(),
        "503 Service Unavailable",
    );
    let child = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        direct_child_start(&cwd, "outage-child")
            .to_string()
            .as_bytes(),
    );
    failed.join().expect("failed child fetch");
    assert!(!child.status.success());
    assert!(child.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&child.stderr).contains("SENTINEL_PRIVATE"));
    assert!(installation.read_snapshot("parent", &cwd).is_err());
    assert!(
        !invoke_pre_tool_raw(
            id,
            &cwd,
            direct_child_tool(&cwd, "outage-child")
                .to_string()
                .as_bytes()
        )
        .stdout
        .is_empty()
    );
}

#[test]
fn parent_refresh_during_child_publication_cannot_emit_stale_context() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let body = publish_v4_parent(&installation, id, &cwd, &listener);
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let delayed = delayed_response(
        listener.try_clone().expect("listener"),
        body.clone(),
        arrived_tx,
        release_rx,
    );
    let child_cwd = cwd.clone();
    let started = thread::spawn(move || {
        invoke_delegated_raw(
            id,
            &child_cwd,
            ClientAdapter::ClaudeV1,
            direct_child_start(&child_cwd, "racing-child")
                .to_string()
                .as_bytes(),
        )
    });
    arrived_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("child fetch arrived");
    let refreshed = response(listener, body, "200 OK");
    assert!(
        invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "resume", "parent")
            .status
            .success()
    );
    refreshed.join().expect("parent refresh");
    release_tx.send(()).expect("release old fetch");
    delayed.join().expect("child fetch");
    let outcome = started.join().expect("child start");
    assert!(!outcome.status.success());
    assert!(outcome.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
    assert!(
        !invoke_pre_tool_raw(
            id,
            &cwd,
            direct_child_tool(&cwd, "racing-child")
                .to_string()
                .as_bytes()
        )
        .stdout
        .is_empty()
    );
}

#[test]
fn concurrent_sibling_starts_publish_independent_child_authority() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let body = publish_v4_parent(&installation, id, &cwd, &listener);
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_a_tx, release_a_rx) = mpsc::channel();
    let (release_b_tx, release_b_rx) = mpsc::channel();
    let delayed_a = delayed_response(
        listener.try_clone().expect("listener"),
        body.clone(),
        arrived_tx.clone(),
        release_a_rx,
    );
    let delayed_b = delayed_response(
        listener.try_clone().expect("listener"),
        body.clone(),
        arrived_tx,
        release_b_rx,
    );
    let starts: Vec<_> = ["child-a", "child-b"]
        .into_iter()
        .map(|agent| {
            let child_cwd = cwd.clone();
            thread::spawn(move || {
                invoke_delegated_raw(
                    id,
                    &child_cwd,
                    ClientAdapter::ClaudeV1,
                    direct_child_start(&child_cwd, agent).to_string().as_bytes(),
                )
            })
        })
        .collect();
    for _ in 0..2 {
        arrived_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("both siblings reached daemon");
    }
    release_a_tx.send(()).expect("release sibling A");
    release_b_tx.send(()).expect("release sibling B");
    delayed_a.join().expect("sibling fetch A");
    delayed_b.join().expect("sibling fetch B");
    for start in starts {
        let output = start.join().expect("child start");
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(!output.stdout.is_empty());
    }
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
    for agent in ["child-a", "child-b"] {
        let reply = response(
            listener.try_clone().expect("listener"),
            body.clone(),
            "200 OK",
        );
        let checked = invoke_pre_tool_raw(
            id,
            &cwd,
            direct_child_tool(&cwd, agent).to_string().as_bytes(),
        );
        assert!(checked.status.success(), "{agent}: {:?}", checked.stderr);
        assert!(checked.stdout.is_empty(), "{agent} should be neutral");
        reply.join().expect("child check");
    }
    let other = root.path().join("other-scope");
    fs::create_dir(&other).expect("different cwd");
    let drifted = invoke_pre_tool_raw(
        id,
        &other,
        direct_child_tool(&other, "child-a").to_string().as_bytes(),
    );
    assert!(!drifted.stdout.is_empty(), "changed child cwd must deny");
}

#[test]
fn killed_child_start_cannot_authorize_a_later_mutation() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    publish_v4_parent(&installation, id, &cwd, &listener);
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let held = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "child request reached daemon");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("child request: {error}"),
            }
        };
        arrived_tx.send(()).expect("arrival");
        release_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("release held request");
        drop(stream);
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("delegated-start")
        .arg("--installation")
        .arg(root.path().join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(ClientAdapter::ClaudeV1.name())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("child helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(
            direct_child_start(&cwd, "killed-child")
                .to_string()
                .as_bytes(),
        )
        .expect("start input");
    arrived_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("child fetch arrived");
    child.kill().expect("kill child helper");
    let stopped = child.wait_with_output().expect("reap child helper");
    release_tx.send(()).expect("release request");
    held.join().expect("held request");
    assert!(!stopped.status.success());
    assert!(stopped.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
    let denied = invoke_pre_tool_raw(
        id,
        &cwd,
        direct_child_tool(&cwd, "killed-child")
            .to_string()
            .as_bytes(),
    );
    assert!(!denied.stdout.is_empty());
}

#[test]
fn root_compaction_requires_a_new_child_id_and_fresh_child_delivery() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::ClaudeV1, port);
    activate_enforced_claude(&installation, root.path());
    let body = publish_v4_parent(&installation, id, &cwd, &listener);
    let first = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let original = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        direct_child_start(&cwd, "before-compact")
            .to_string()
            .as_bytes(),
    );
    first.join().expect("original child fetch");
    assert!(original.status.success());
    let second = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let compacted = invoke_session(id, &cwd, ClientAdapter::ClaudeV1, "compact", "parent");
    second.join().expect("compact fetch");
    assert!(compacted.status.success(), "{:?}", compacted.stderr);
    let stale = invoke_pre_tool_raw(
        id,
        &cwd,
        direct_child_tool(&cwd, "before-compact")
            .to_string()
            .as_bytes(),
    );
    assert!(!stale.stdout.is_empty(), "old lineage must deny");
    let repeated = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        direct_child_start(&cwd, "before-compact")
            .to_string()
            .as_bytes(),
    );
    assert!(!repeated.status.success());
    assert!(repeated.stdout.is_empty());
    let third = response(
        listener.try_clone().expect("listener"),
        body.clone(),
        "200 OK",
    );
    let replacement = invoke_delegated_raw(
        id,
        &cwd,
        ClientAdapter::ClaudeV1,
        direct_child_start(&cwd, "after-compact")
            .to_string()
            .as_bytes(),
    );
    third.join().expect("replacement child fetch");
    assert!(replacement.status.success(), "{:?}", replacement.stderr);
    let fourth = response(listener, body, "200 OK");
    let new_child = invoke_pre_tool_raw(
        id,
        &cwd,
        direct_child_tool(&cwd, "after-compact")
            .to_string()
            .as_bytes(),
    );
    fourth.join().expect("new child check");
    assert!(new_child.status.success());
    assert!(new_child.stdout.is_empty());
    assert!(installation.read_snapshot("parent", &cwd).is_ok());
}

#[test]
fn unsupported_child_start_is_explicit_and_keeps_root_authority() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let (installation, id, cwd) = configured(root.path(), adapter, 55489);
        let pending = installation.begin_session("parent").expect("pending");
        installation
            .attach_binding(&pending, &cwd)
            .expect("binding");
        installation
            .complete_session(&pending, &cwd, pack(&installation, &cwd), &mut Vec::new())
            .expect("root emission");
        let mut event = serde_json::json!({
            "hook_event_name": "SubagentStart",
            "session_id": "parent",
            "agent_id": "child",
            "agent_type": "Explore",
            "cwd": cwd,
        });
        if adapter == ClientAdapter::CodexV1 {
            event["turn_id"] = serde_json::json!("turn");
        }
        let output = invoke_delegated_raw(id, &cwd, adapter, event.to_string().as_bytes());
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"delegated_start: status=advisory_only code=unsupported_capability\n"
        );
        assert!(installation.read_snapshot("parent", &cwd).is_ok());
        event["agent_id"] = serde_json::Value::Null;
        event["agent_type"] = serde_json::json!("SENTINEL_PRIVATE_TYPE");
        let malformed = invoke_delegated_raw(id, &cwd, adapter, event.to_string().as_bytes());
        assert!(!malformed.status.success());
        assert!(malformed.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&malformed.stderr).contains("SENTINEL"));
        assert!(installation.read_snapshot("parent", &cwd).is_ok());
    }
}

fn invoke_raw_with_env(
    id: Uuid,
    cwd: &Path,
    adapter: ClientAdapter,
    event: &[u8],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("session-start")
        .arg("--installation")
        .arg(cwd.parent().expect("fixture root").join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(adapter.name())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(env.iter().copied())
        .spawn()
        .expect("spawn helper");
    let written = child.stdin.take().expect("stdin").write_all(event);
    if let Err(error) = written {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().expect("helper output")
}

#[test]
fn invalid_but_identifiable_event_retires_head_without_http() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::CodexV1, port);
    let pack = pack(&installation, &cwd);
    let handle = response(
        listener.try_clone().expect("clone listener"),
        serde_json::to_vec(&pack).expect("pack JSON"),
        "200 OK",
    );
    assert!(
        invoke(id, &cwd, ClientAdapter::CodexV1, "startup")
            .status
            .success()
    );
    handle.join().expect("server");
    installation
        .read_snapshot("session", &cwd)
        .expect("emitted");

    let event = serde_json::json!({
        "hook_event_name": "SessionStart", "session_id": "session", "cwd": cwd,
        "source": "fork"
    });
    let output = invoke_raw(
        id,
        &cwd,
        ClientAdapter::CodexV1,
        event.to_string().as_bytes(),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("stage=event"));
    assert!(diagnostic.contains("generation="));
    assert!(installation.read_snapshot("session", &cwd).is_err());
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    assert!(
        listener.accept().is_err(),
        "unsupported event made an HTTP request"
    );

    for bytes in [
        br#"{"session_id":"session","session_id":"ambiguous"}"#.as_slice(),
        br#"{"session_id":12}"#.as_slice(),
        b"\xff".as_slice(),
        b"{}{}".as_slice(),
    ] {
        let output = invoke_raw(id, &cwd, ClientAdapter::CodexV1, bytes);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    assert!(
        listener.accept().is_err(),
        "malformed event made an HTTP request"
    );
}

#[test]
fn both_clients_emit_exact_fresh_publications_for_each_source() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            if source == "fork" && adapter == ClientAdapter::CodexV1 {
                continue;
            }
            let root = private_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("address").port();
            let (installation, id, cwd) = configured(root.path(), adapter, port);
            let pack = pack(&installation, &cwd);
            let expected = pack.publication().expect("publication");
            let handle = response(
                listener,
                serde_json::to_vec(&pack).expect("pack JSON"),
                "200 OK",
            );
            let output = invoke(id, &cwd, adapter, source);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            assert_eq!(output.stdout.last(), Some(&b'\n'));
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON");
            assert_eq!(json["hookSpecificOutput"]["hookEventName"], "SessionStart");
            assert_eq!(json["hookSpecificOutput"]["additionalContext"], expected);
            assert_eq!(
                installation
                    .read_snapshot("session", &cwd)
                    .expect("reader")
                    .pack,
                pack
            );
            let request = handle.join().expect("server");
            assert!(request.starts_with("GET /api/v1/projects/general/guardrails?context="));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-token")
            );
        }
    }
}

#[test]
fn both_adapters_preserve_general_and_project_policy_fields_exactly() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let config_path = root.path().join("config.toml");
        let source = fs::read_to_string(&config_path).expect("config");
        fs::write(
            &config_path,
            source.replace(
                "guardrails_project = \"general\"",
                "guardrails_project = \"app\"",
            ),
        )
        .expect("project config");
        installation
            .activate(&config_path)
            .expect("project activation");
        let binding = installation
            .active()
            .expect("active")
            .config
            .resolve(&cwd)
            .expect("binding");
        let rules = vec![
            CanonicalRule {
                project: "general".to_owned(),
                id: Uuid::from_u128(41),
                policy_key: Some("build.storage".to_owned()),
                revision: Some(7),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: BTreeMap::from([("build.target".to_owned(), "persistent-disk".to_owned())]),
                content: "General: \"quoted\" \\ path\r\n🦀 e\u{301}".to_owned(),
                overrides: None,
            },
            CanonicalRule {
                project: "app".to_owned(),
                id: Uuid::from_u128(42),
                policy_key: Some("review.mode".to_owned()),
                revision: Some(9),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors {
                    profile: Some(BTreeSet::from(["workstation".to_owned()])),
                    ..PolicySelectors::default()
                },
                values: BTreeMap::from([("review.mode".to_owned(), "strict".to_owned())]),
                content: "Project: first\nsecond\t\u{0001}終".to_owned(),
                overrides: None,
            },
        ];
        let pack = GuardrailPack::new("app".to_owned(), binding.context().clone(), 1, rules)
            .expect("rich pack");
        let expected = pack.publication().expect("canonical publication");
        let handle = response(listener, serde_json::to_vec(&pack).expect("pack"), "200 OK");
        let output = invoke(id, &cwd, adapter, "startup");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        assert!(!output.stdout[..output.stdout.len() - 1].contains(&b'\n'));
        let decoded: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON");
        assert_eq!(
            decoded["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        assert_eq!(decoded["hookSpecificOutput"]["additionalContext"], expected);
        let snapshot = installation.read_snapshot("session", &cwd).expect("reader");
        assert_eq!(snapshot.pack, pack);
        assert_eq!(
            snapshot.binding,
            binding.public_json().expect("public binding")
        );
        assert!(
            handle
                .join()
                .expect("request")
                .starts_with("GET /api/v1/projects/app/guardrails?context=")
        );
    }
}

#[test]
fn repeated_supported_starts_fetch_and_commit_distinct_generations() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let mut first = pack(&installation, &cwd);
        first.mandatory[0].content.push_str("\nEND-MARKER-1");
        let first = GuardrailPack::new(
            first.project,
            first.context,
            first.resolver_schema_version,
            first.mandatory,
        )
        .expect("first pack");
        let mut next = first.clone();
        next.mandatory[0].revision = Some(2);
        next.mandatory[0].content.push_str("\nEND-MARKER-2");
        let next = GuardrailPack::new(
            next.project,
            next.context,
            next.resolver_schema_version,
            next.mandatory,
        )
        .expect("successor pack");
        assert_ne!(first.digest, next.digest);
        let sources: &[&str] = if adapter == ClientAdapter::CodexV1 {
            &["startup", "resume", "clear", "compact", "startup"]
        } else {
            &["startup", "resume", "clear", "compact", "fork", "startup"]
        };
        let mut previous = None;
        for (index, source) in sources.iter().enumerate() {
            let current = if index + 1 == sources.len() {
                &next
            } else {
                &first
            };
            let handle = response(
                listener.try_clone().expect("listener clone"),
                serde_json::to_vec(current).expect("pack JSON"),
                "200 OK",
            );
            let output = invoke(id, &cwd, adapter, source);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            assert_eq!(output.stdout.last(), Some(&b'\n'));
            assert!(!output.stdout[..output.stdout.len() - 1].contains(&b'\n'));
            let decoded: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("single output object");
            assert_eq!(
                decoded["hookSpecificOutput"]["additionalContext"],
                current.publication().expect("publication")
            );
            assert_eq!(
                decoded["hookSpecificOutput"]["hookEventName"],
                "SessionStart"
            );
            let snapshot = installation
                .read_snapshot("session", &cwd)
                .expect("current public snapshot");
            assert_eq!(snapshot.pack, *current);
            assert_ne!(Some(snapshot.generation), previous);
            previous = Some(snapshot.generation);
            let request = handle.join().expect("one HTTP request");
            assert_eq!(request.matches("GET ").count(), 1);
            assert!(request.starts_with("GET /api/v1/projects/general/guardrails?context="));
            assert!(!request.contains("/recall"));
            assert!(!request.contains("/bootstrap"));
            assert!(!request.contains("/sessions"));
        }
    }
}

fn delayed_response(
    listener: TcpListener,
    body: Vec<u8>,
    arrived: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "first fetch reached daemon");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("HTTP accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("request timeout");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).expect("request read");
            assert!(count > 0, "complete request");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        arrived.send(()).expect("arrival signal");
        release
            .recv_timeout(Duration::from_secs(8))
            .expect("release signal");
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .expect("response header");
        stream.write_all(&body).expect("response body");
        String::from_utf8(request).expect("request UTF-8")
    })
}

#[test]
fn older_fetch_cannot_restore_or_replace_a_newer_same_session_claim() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for newer_emits in [false, true] {
            let root = private_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("address").port();
            let (installation, id, cwd) = configured(root.path(), adapter, port);
            let expected = pack(&installation, &cwd);
            let (arrived_tx, arrived_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let first_listener = listener.try_clone().expect("listener clone");
            let first_body = serde_json::to_vec(&expected).expect("pack JSON");
            let first_server = delayed_response(first_listener, first_body, arrived_tx, release_rx);
            let first_cwd = cwd.clone();
            let first = thread::spawn(move || invoke(id, &first_cwd, adapter, "startup"));
            arrived_rx
                .recv_timeout(Duration::from_secs(8))
                .expect("first request arrived");
            assert!(installation.read_snapshot("session", &cwd).is_err());

            let newer_snapshot = if newer_emits {
                let second_server = response(
                    listener.try_clone().expect("listener clone"),
                    serde_json::to_vec(&expected).expect("pack JSON"),
                    "200 OK",
                );
                let output = invoke(id, &cwd, adapter, "resume");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(output.stderr.is_empty());
                let decoded: serde_json::Value =
                    serde_json::from_slice(&output.stdout).expect("one output object");
                assert_eq!(
                    decoded["hookSpecificOutput"]["additionalContext"],
                    expected.publication().expect("publication")
                );
                let request = second_server.join().expect("second request");
                assert_eq!(request.matches("GET ").count(), 1);
                Some(
                    installation
                        .read_snapshot("session", &cwd)
                        .expect("newer emitted snapshot"),
                )
            } else {
                let output = invoke(id, &cwd, adapter, "unsupported");
                assert!(!output.status.success());
                assert!(output.stdout.is_empty());
                assert!(installation.read_snapshot("session", &cwd).is_err());
                None
            };
            release_tx.send(()).expect("release first fetch");
            let first_request = first_server.join().expect("first request");
            assert_eq!(first_request.matches("GET ").count(), 1);
            let first_output = first.join().expect("first helper");
            assert!(!first_output.status.success());
            assert!(first_output.stdout.is_empty());
            if let Some(newer) = newer_snapshot {
                let current = installation
                    .read_snapshot("session", &cwd)
                    .expect("newer remains current");
                assert_eq!(current.generation, newer.generation);
                assert_eq!(current.pack, expected);
            } else {
                assert!(installation.read_snapshot("session", &cwd).is_err());
            }
        }
    }
}

#[test]
fn both_adapters_reject_fresh_transport_failures_without_old_snapshot_fallback() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let handle = response(
            listener.try_clone().expect("listener clone"),
            serde_json::to_vec(&good).expect("pack JSON"),
            "200 OK",
        );
        let success = invoke(id, &cwd, adapter, "startup");
        assert!(success.status.success());
        handle.join().expect("initial request");
        assert!(installation.read_snapshot("session", &cwd).is_ok());

        for (name, status, body) in invalid_pack_responses(&good) {
            let handle = response(listener.try_clone().expect("listener clone"), body, status);
            let output = invoke(id, &cwd, adapter, "resume");
            assert!(!output.status.success(), "{name}");
            assert!(output.stdout.is_empty(), "{name}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("stage=fetch"), "{name}: {stderr}");
            assert!(!stderr.contains("fixture-token"), "{name}");
            assert!(
                installation.read_snapshot("session", &cwd).is_err(),
                "{name}"
            );
            let request = handle.join().expect("fresh HTTP request");
            assert_eq!(request.matches("GET ").count(), 1, "{name}");
        }
        let handle = response(listener, serde_json::to_vec(&good).expect("pack"), "200 OK");
        let recovered = invoke(id, &cwd, adapter, "clear");
        assert!(recovered.status.success());
        handle.join().expect("recovery request");
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("recovered")
                .pack,
            good
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "pre-identity and post-claim event failures share one retained-head fixture"
)]
fn input_identity_and_exact_byte_bounds_control_retirement() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let handle = response(
            listener.try_clone().expect("listener clone"),
            serde_json::to_vec(&good).expect("pack"),
            "200 OK",
        );
        assert!(invoke(id, &cwd, adapter, "startup").status.success());
        handle.join().expect("initial request");
        let prior = installation
            .read_snapshot("session", &cwd)
            .expect("initial snapshot")
            .generation;

        let pre_identity = [
            br#"{"hook_event_name":"SessionStart","source":"resume"}"#.to_vec(),
            br#"{"session_id":"session","session_id":"other"}"#.to_vec(),
            br#"{"session_id":null}"#.to_vec(),
            br#"{"session_id":123}"#.to_vec(),
            br#"{"session_id":""}"#.to_vec(),
            b"\xff".to_vec(),
            b"{}{}".to_vec(),
            b"{not-json".to_vec(),
            serde_json::json!({"session_id":"s".repeat(1025),"cwd":cwd,
                "hook_event_name":"SessionStart","source":"resume"})
            .to_string()
            .into_bytes(),
            vec![b'x'; memory_hooks::session_start::EVENT_BYTES + 1],
        ];
        for raw in pre_identity {
            let output = invoke_raw(id, &cwd, adapter, &raw);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(output.stderr.len() < 1024);
            assert_eq!(
                installation
                    .read_snapshot("session", &cwd)
                    .expect("pre-identity rejection retains prior head")
                    .generation,
                prior
            );
        }

        let mut at_limit = serde_json::json!({
            "hook_event_name":"SessionStart", "session_id":"session", "cwd":cwd,
            "source":"resume", "project":"SENTINEL_EVENT_PROJECT",
            "profile":"SENTINEL_EVENT_PROFILE", "memoryd_url":"SENTINEL_EVENT_URL",
            "metadata":""
        });
        let overhead = at_limit.to_string().len();
        at_limit["metadata"] = serde_json::Value::String(
            "x".repeat(memory_hooks::session_start::EVENT_BYTES - overhead),
        );
        let exact = at_limit.to_string().into_bytes();
        assert_eq!(exact.len(), memory_hooks::session_start::EVENT_BYTES);
        let handle = response(
            listener.try_clone().expect("listener clone"),
            serde_json::to_vec(&good).expect("pack"),
            "200 OK",
        );
        let output = invoke_raw(id, &cwd, adapter, &exact);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let request = handle.join().expect("exact-limit request");
        assert!(request.starts_with("GET /api/v1/projects/general/guardrails?context="));
        assert!(!request.contains("SENTINEL_EVENT"));
        assert_ne!(
            installation
                .read_snapshot("session", &cwd)
                .expect("new generation")
                .generation,
            prior
        );

        let max_session = "é".repeat(512);
        assert_eq!(max_session.len(), 1024);
        let max_event = serde_json::json!({"session_id":max_session.clone(), "cwd":cwd,
            "hook_event_name":"SessionStart", "source":"startup"});
        let handle = response(
            listener.try_clone().expect("listener clone"),
            serde_json::to_vec(&good).expect("pack"),
            "200 OK",
        );
        let output = invoke_raw(id, &cwd, adapter, max_event.to_string().as_bytes());
        assert!(output.status.success());
        handle.join().expect("max-session request");
        let max_generation = installation
            .read_snapshot(&max_session, &cwd)
            .expect("1024-byte session")
            .generation;
        let over_session = serde_json::json!({"session_id":format!("{max_session}x"),
            "cwd":cwd, "hook_event_name":"SessionStart", "source":"resume"});
        let rejected = invoke_raw(id, &cwd, adapter, over_session.to_string().as_bytes());
        assert!(!rejected.status.success());
        assert!(rejected.stdout.is_empty());
        assert_eq!(
            installation
                .read_snapshot(&max_session, &cwd)
                .expect("overlong identity cannot retire other session")
                .generation,
            max_generation
        );

        let other = root.path().join("other-cwd");
        fs::create_dir(&other).expect("other cwd");
        let root_generation = installation
            .read_snapshot("session", &cwd)
            .expect("root before child-shaped event")
            .generation;
        for (field, value) in [
            ("agent_id", serde_json::json!("child")),
            ("agent_id", serde_json::Value::Null),
            ("is_subagent", serde_json::json!(false)),
            ("parent_session_id", serde_json::json!("parent")),
        ] {
            let mut event = serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":"resume"});
            event[field] = value;
            let output = invoke_raw(id, &cwd, adapter, event.to_string().as_bytes());
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                installation
                    .read_snapshot("session", &cwd)
                    .expect("child-shaped event keeps parent")
                    .generation,
                root_generation
            );
        }
        let identifiable = [
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"Other", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":"refresh"}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":null}),
            serde_json::json!({"session_id":"session", "cwd":other,
                "hook_event_name":"SessionStart", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":"relative",
                "hook_event_name":"SessionStart", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":null,
                "hook_event_name":"SessionStart", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":root.path().join("absent"),
                "hook_event_name":"SessionStart", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart"}),
        ];
        for event in identifiable {
            let output = invoke_raw(id, &cwd, adapter, event.to_string().as_bytes());
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("stage=event"));
            assert!(installation.read_snapshot("session", &cwd).is_err());
        }
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        assert!(listener.accept().is_err(), "invalid input reached HTTP");
    }
}

#[test]
fn both_adapters_reject_partial_oversized_and_redirected_http_bodies() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let bytes = serde_json::to_vec(&good).expect("pack JSON");
        let redirect_target = TcpListener::bind("127.0.0.1:0").expect("redirect target");
        let target_port = redirect_target.local_addr().expect("target address").port();
        redirect_target
            .set_nonblocking(true)
            .expect("nonblocking redirect target");
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        let declared = "HTTP/1.1 200 OK\r\nContent-Length: 32769\r\nConnection: close\r\n\r\n"
            .as_bytes()
            .to_vec();
        let mut streamed = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            32 * 1024 + 1
        )
        .into_bytes();
        streamed.extend(vec![b'x'; 32 * 1024 + 1]);
        streamed.extend_from_slice(b"\r\n0\r\n\r\n");
        let mut truncated = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len() + 1
        )
        .into_bytes();
        truncated.extend_from_slice(&bytes);
        let mut no_length = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        no_length.extend_from_slice(&bytes[..bytes.len() - 1]);
        for (name, wire) in [
            ("declared_over", declared),
            ("streamed_over", streamed),
            ("truncated", truncated),
            ("no_length_partial", no_length),
            ("redirect", redirect),
        ] {
            let control = response(
                listener.try_clone().expect("listener clone"),
                bytes.clone(),
                "200 OK",
            );
            let success = invoke(id, &cwd, adapter, "startup");
            assert!(success.status.success(), "control for {name}");
            control.join().expect("control request");
            assert!(installation.read_snapshot("session", &cwd).is_ok());

            let handle = response_wire(listener.try_clone().expect("listener clone"), wire);
            let failure = invoke(id, &cwd, adapter, "resume");
            assert!(!failure.status.success(), "{name}");
            assert!(failure.stdout.is_empty(), "{name}");
            let diagnostic = String::from_utf8_lossy(&failure.stderr);
            assert!(diagnostic.contains("stage=fetch"), "{name}: {diagnostic}");
            assert!(!diagnostic.contains("fixture-token"), "{name}");
            assert!(
                installation.read_snapshot("session", &cwd).is_err(),
                "{name}"
            );
            let request = handle.join().expect("fresh request");
            assert_eq!(request.matches("GET ").count(), 1, "{name}");
            if name == "redirect" {
                assert!(redirect_target.accept().is_err(), "redirect was followed");
            }
        }

        let mut at_limit = bytes;
        let padding = 32 * 1024 - at_limit.len();
        at_limit.extend(vec![b' '; padding]);
        assert_eq!(at_limit.len(), 32 * 1024);
        let handle = response(listener, at_limit, "200 OK");
        let recovered = invoke(id, &cwd, adapter, "clear");
        assert!(recovered.status.success());
        assert!(recovered.stderr.is_empty());
        handle.join().expect("exact HTTP limit request");
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("recovered exact-limit body")
                .pack,
            good
        );
    }
}

#[test]
fn stalled_headers_and_body_expire_the_fixed_transport_timeout_for_both_adapters() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let body = serde_json::to_vec(&good).expect("pack JSON");
        for (name, prefix) in [
            ("headers", b"HTTP/1.1 200 OK\r\nContent-".to_vec()),
            (
                "body",
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes(),
            ),
        ] {
            let control = response(
                listener.try_clone().expect("listener clone"),
                body.clone(),
                "200 OK",
            );
            let success = invoke(id, &cwd, adapter, "startup");
            assert!(success.status.success(), "control for {name}");
            control.join().expect("control request");
            assert!(installation.read_snapshot("session", &cwd).is_ok());

            let (release_tx, release_rx) = mpsc::channel();
            let stalled = stalled_response(
                listener.try_clone().expect("listener clone"),
                prefix,
                release_rx,
            );
            let started = Instant::now();
            let failure = invoke(id, &cwd, adapter, "resume");
            let elapsed = started.elapsed();
            release_tx.send(()).expect("release stalled response");
            let request = stalled.join().expect("stalled request");
            assert_eq!(request.matches("GET ").count(), 1, "{name}");
            assert!(!failure.status.success(), "{name}");
            assert!(failure.stdout.is_empty(), "{name}");
            assert!(
                String::from_utf8_lossy(&failure.stderr).contains("stage=fetch"),
                "{name}"
            );
            assert!(elapsed >= Duration::from_secs(4), "{name}: {elapsed:?}");
            assert!(elapsed < Duration::from_secs(8), "{name}: {elapsed:?}");
            assert!(installation.read_snapshot("session", &cwd).is_err(), "{name}");
        }
    }
}

#[test]
fn chunked_body_success_keeps_exact_publication_and_reader_evidence() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let body = serde_json::to_vec(&good).expect("pack JSON");
        let midpoint = body.len() / 2;
        let mut wire = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for chunk in [&body[..midpoint], &body[midpoint..]] {
            wire.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");
        let handle = response_wire(listener, wire);
        let output = invoke(id, &cwd, adapter, "startup");
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        let decoded: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("one output object");
        assert_eq!(
            decoded["hookSpecificOutput"]["additionalContext"],
            good.publication().expect("canonical publication")
        );
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("public reader")
                .pack,
            good
        );
        assert_eq!(
            handle.join().expect("HTTP request").matches("GET ").count(),
            1
        );
    }
}

#[test]
fn connection_refusal_reset_and_disconnect_retire_prior_delivery() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for failure in ["refused", "reset", "disconnect"] {
            let root = private_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("address").port();
            let (installation, id, cwd) = configured(root.path(), adapter, port);
            let good = pack(&installation, &cwd);
            let control = response(
                listener.try_clone().expect("listener clone"),
                serde_json::to_vec(&good).expect("pack JSON"),
                "200 OK",
            );
            assert!(invoke(id, &cwd, adapter, "startup").status.success());
            assert_eq!(
                control
                    .join()
                    .expect("control request")
                    .matches("GET ")
                    .count(),
                1
            );
            assert!(installation.read_snapshot("session", &cwd).is_ok());

            let failed_peer = match failure {
                "refused" => {
                    drop(listener);
                    None
                }
                "reset" => Some(reset_response(listener)),
                "disconnect" => Some(response_wire(listener, Vec::new())),
                _ => unreachable!(),
            };
            let output = invoke(id, &cwd, adapter, "resume");
            assert!(!output.status.success(), "{failure}");
            assert!(output.stdout.is_empty(), "{failure}");
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(
                diagnostic.contains("stage=fetch"),
                "{failure}: {diagnostic}"
            );
            assert!(!diagnostic.contains("fixture-token"), "{failure}");
            assert!(
                installation.read_snapshot("session", &cwd).is_err(),
                "{failure}"
            );
            if let Some(peer) = failed_peer {
                assert_eq!(peer.join().expect("failed peer").matches("GET ").count(), 1);
            }
        }
    }
}

#[test]
fn invalid_protected_origins_make_zero_requests_for_both_adapters() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for suffix in [
            "/memoryd",
            "/memoryd/",
            "//",
            "/%2f",
            "/.",
            "/..",
            "/./",
            "\\memoryd",
            "/?query=1",
            "/#fragment",
            "/ ",
            "/\n",
        ] {
            let root = private_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("address").port();
            let (installation, id, cwd) = configured(root.path(), adapter, port);
            let config = root.path().join("config.toml");
            let original = fs::read_to_string(&config).expect("protected config");
            let valid = format!("memoryd_url = \"http://127.0.0.1:{port}/\"");
            let invalid = format!(
                "memoryd_url = {:?}",
                format!("http://127.0.0.1:{port}{suffix}")
            );
            assert!(original.contains(&valid));
            fs::write(&config, original.replace(&valid, &invalid)).expect("config replacement");
            let output = invoke(id, &cwd, adapter, "startup");
            assert!(!output.status.success(), "{suffix:?}");
            assert!(output.stdout.is_empty(), "{suffix:?}");
            assert!(output.stderr.len() < 1024, "{suffix:?}");
            assert!(installation.read_snapshot("session", &cwd).is_err());
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            assert!(listener.accept().is_err(), "{suffix:?} reached HTTP");
        }
    }
}

#[test]
fn child_proxy_variables_cannot_reroute_or_observe_mandatory_bearer() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("mandatory listener");
        let port = listener.local_addr().expect("mandatory address").port();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").expect("proxy listener");
        let proxy_port = proxy_listener.local_addr().expect("proxy address").port();
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let handle = response(
            listener.try_clone().expect("mandatory clone"),
            serde_json::to_vec(&good).expect("pack"),
            "200 OK",
        );
        let event = serde_json::json!({"session_id":"session", "cwd":cwd,
            "hook_event_name":"SessionStart", "source":"startup"});
        let output = invoke_raw_with_env(
            id,
            &cwd,
            adapter,
            event.to_string().as_bytes(),
            &[
                ("HTTP_PROXY", &proxy_url),
                ("HTTPS_PROXY", &proxy_url),
                ("ALL_PROXY", &proxy_url),
                ("http_proxy", &proxy_url),
                ("https_proxy", &proxy_url),
                ("all_proxy", &proxy_url),
                ("NO_PROXY", ""),
                ("no_proxy", ""),
            ],
        );
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("mandatory delivery")
                .pack,
            good
        );
        let request = handle.join().expect("mandatory request");
        assert!(request.starts_with("GET /api/v1/projects/general/guardrails?context="));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-token")
        );
        proxy_listener
            .set_nonblocking(true)
            .expect("proxy nonblocking");
        assert!(
            proxy_listener.accept().is_err(),
            "proxy received bearer request"
        );
        listener
            .set_nonblocking(true)
            .expect("mandatory nonblocking");
        assert!(
            listener.accept().is_err(),
            "unexpected optional HTTP request"
        );
    }
}

#[test]
fn both_adapters_send_exact_encoded_project_and_complete_trusted_context() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let config = root.path().join("config.toml");
        let initial = fs::read_to_string(&config).expect("protected config");
        let scoped = initial
            .replace(
                "guardrails_project = \"general\"",
                "guardrails_project = \"ops/a b\"",
            )
            .replace(
                "profile = \"workstation\"",
                "profile = \"workstation\"\nphase = \"reworking\"\nlanguage = [\"rust\", \"python\"]\ntool = []",
            );
        let pathless = scoped.replace(&format!("{port}/"), &port.to_string());
        let unknown_tool = scoped.replace("tool = []\n", "");
        for (index, source) in [scoped.as_str(), pathless.as_str(), unknown_tool.as_str()]
            .iter()
            .enumerate()
        {
            fs::write(&config, source).expect("protected config update");
            installation.activate(&config).expect("activate scope");
            assert!(installation.read_snapshot("session", &cwd).is_err());
            let expected = pack(&installation, &cwd);
            assert_eq!(expected.project, "ops/a b");
            assert_eq!(expected.context.profile.as_deref(), Some("workstation"));
            assert_eq!(expected.context.phase.as_deref(), Some("reworking"));
            assert_eq!(
                expected
                    .context
                    .language
                    .as_ref()
                    .map(std::collections::BTreeSet::len),
                Some(2)
            );
            assert_eq!(
                expected
                    .context
                    .tool
                    .as_ref()
                    .map(std::collections::BTreeSet::len),
                if index == 2 { None } else { Some(0) }
            );
            let handle = response(
                listener.try_clone().expect("listener clone"),
                serde_json::to_vec(&expected).expect("pack"),
                "200 OK",
            );
            let output = invoke(
                id,
                &cwd,
                adapter,
                if index == 0 { "startup" } else { "resume" },
            );
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            let decoded: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("output JSON");
            assert_eq!(
                decoded["hookSpecificOutput"]["additionalContext"],
                expected.publication().expect("publication")
            );
            assert_eq!(
                installation
                    .read_snapshot("session", &cwd)
                    .expect("public reader")
                    .pack,
                expected
            );
            let request = handle.join().expect("request");
            let first_line = request.lines().next().expect("request line");
            let target = first_line
                .split_whitespace()
                .nth(1)
                .expect("request target");
            let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}{target}"))
                .expect("request URL");
            assert_eq!(url.path(), "/api/v1/projects/ops%2Fa%20b/guardrails");
            let context: Vec<_> = url
                .query_pairs()
                .filter(|(name, _)| name == "context")
                .map(|(_, value)| value.into_owned())
                .collect();
            assert_eq!(context.len(), 1);
            let decoded_context: memory_common::policy::ResolutionContext =
                serde_json::from_str(&context[0]).expect("context JSON");
            assert_eq!(decoded_context, expected.context);
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-token")
            );
        }
    }
}

fn exact_reachable_pack(binding: &memory_hooks::ResolvedBinding) -> GuardrailPack {
    let make = |fill: usize| {
        let rules = (0..u32::try_from(MAX_POLICIES).expect("rule count"))
            .map(|index| CanonicalRule {
                project: "general".to_owned(),
                id: Uuid::from_u128(u128::from(index) + 1),
                policy_key: Some(format!("boundary-{index:02}")),
                revision: Some(i64::from(index) + 1),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: BTreeMap::new(),
                content: format!(
                    "Rüle {index}: \"\\\r\n\t🦀e\u{301}{} END-MARKER-{index}",
                    "\u{0001}".repeat(fill)
                ),
                overrides: None,
            })
            .collect();
        GuardrailPack::new(
            binding.project().to_owned(),
            binding.context().clone(),
            1,
            rules,
        )
    };
    assert!(make(0).is_ok(), "{:?}", make(0));
    let mut low: usize = 0;
    let mut high: usize = 512;
    while low + 1 < high {
        let middle = low.midpoint(high);
        if make(middle).is_ok() {
            low = middle;
        } else {
            high = middle;
        }
    }
    let near = make(low).expect("largest reachable pack");
    assert!(make(low + 1).is_err());
    let mut pack = near.clone();
    for suffix_bytes in 1..=64 {
        let mut rules = near.mandatory.clone();
        rules[0].content.push_str(&"x".repeat(suffix_bytes));
        let Ok(candidate) = GuardrailPack::new(
            near.project.clone(),
            near.context.clone(),
            near.resolver_schema_version,
            rules,
        ) else {
            break;
        };
        pack = candidate;
    }
    let mut over = pack.mandatory.clone();
    over[0].content.push('x');
    assert!(
        GuardrailPack::new(
            pack.project.clone(),
            pack.context.clone(),
            pack.resolver_schema_version,
            over,
        )
        .is_err()
    );
    assert_eq!(pack.mandatory.len(), MAX_POLICIES);
    let canonical = pack.canonical_bytes().expect("canonical bytes");
    let serialized = serde_json::to_vec(&pack).expect("serialized pack");
    let publication = pack.publication().expect("publication");
    let slack = [
        MAX_CANONICAL_BYTES - canonical.len(),
        MAX_PUBLICATION_BYTES - serialized.len(),
        MAX_PUBLICATION_BYTES - publication.len(),
    ];
    assert!(
        slack.contains(&0),
        "reachable byte limit must be exact: {slack:?}"
    );
    pack
}

#[test]
fn both_adapters_deliver_all_rules_at_the_reachable_shared_byte_boundary() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let binding = installation
            .active()
            .expect("active")
            .config
            .resolve(&cwd)
            .expect("binding");
        let pack = exact_reachable_pack(&binding);
        let serialized = serde_json::to_vec(&pack).expect("serialized pack");
        let publication = pack.publication().expect("publication");
        assert!(publication.contains("END-MARKER-31"));
        let handle = response(listener, serialized, "200 OK");
        let output = invoke(id, &cwd, adapter, "startup");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        let decoded: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("single output");
        assert_eq!(
            decoded["hookSpecificOutput"]["additionalContext"],
            publication
        );
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("reader")
                .pack,
            pack
        );
        handle.join().expect("HTTP request");
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "maximum context dimensions and round-trip assertions share one fixture"
)]
fn both_adapters_deliver_maximum_trusted_context_dimensions() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let config = root.path().join("config.toml");
        let original = fs::read_to_string(&config).expect("config");
        let values = |prefix: char| {
            (0..32)
                .map(|index| format!("{prefix}{index:02}{}", "x".repeat(125)))
                .collect::<Vec<_>>()
        };
        let languages = values('l');
        let tools = values('t');
        assert!(languages.iter().all(|value| value.len() == 128));
        let array = |items: &[String]| {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| format!("{item:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let project = "p".repeat(256);
        let context = format!(
            "profile = {:?}\nphase = {:?}\nlanguage = {}\ntool = {}",
            format!("p{}", "x".repeat(127)),
            format!("r{}", "x".repeat(127)),
            array(&languages),
            array(&tools)
        );
        let source = original
            .replace(
                "guardrails_project = \"general\"",
                &format!("guardrails_project = {project:?}"),
            )
            .replace("profile = \"workstation\"", &context);
        fs::write(&config, source).expect("maximal context config");
        installation.activate(&config).expect("activate");
        let expected = pack(&installation, &cwd);
        assert_eq!(expected.project.len(), 256);
        assert_eq!(
            expected.context.profile.as_ref().map(String::len),
            Some(128)
        );
        assert_eq!(expected.context.phase.as_ref().map(String::len), Some(128));
        assert_eq!(
            expected
                .context
                .language
                .as_ref()
                .map(std::collections::BTreeSet::len),
            Some(32)
        );
        assert_eq!(
            expected
                .context
                .tool
                .as_ref()
                .map(std::collections::BTreeSet::len),
            Some(32)
        );
        let serialized = serde_json::to_vec(&expected).expect("pack JSON");
        assert!(serialized.len() <= MAX_PUBLICATION_BYTES);
        let publication = expected.publication().expect("publication");
        let handle = response(listener, serialized, "200 OK");
        let output = invoke(id, &cwd, adapter, "startup");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let decoded: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("single output");
        assert_eq!(
            decoded["hookSpecificOutput"]["additionalContext"],
            publication
        );
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("reader")
                .pack,
            expected
        );
        let request = handle.join().expect("HTTP request");
        let target = request
            .lines()
            .next()
            .expect("request line")
            .split_whitespace()
            .nth(1)
            .expect("request target");
        let url =
            reqwest::Url::parse(&format!("http://127.0.0.1:{port}{target}")).expect("request URL");
        let context_query = url
            .query_pairs()
            .find(|(name, _)| name == "context")
            .expect("context query")
            .1;
        let decoded_context: memory_common::policy::ResolutionContext =
            serde_json::from_str(&context_query).expect("context JSON");
        assert_eq!(decoded_context, expected.context);
    }
}

#[test]
fn broken_real_stdout_pipe_never_commits_emitted_evidence() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), adapter, port);
        let good = pack(&installation, &cwd);
        let handle = response(
            listener.try_clone().expect("listener clone"),
            serde_json::to_vec(&good).expect("pack"),
            "200 OK",
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
            .arg("session-start")
            .arg("--installation")
            .arg(root.path().join("control"))
            .arg("--installation-id")
            .arg(id.to_string())
            .arg("--client")
            .arg(adapter.name())
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("helper");
        drop(child.stdout.take());
        let event = serde_json::json!({"session_id":"session", "cwd":cwd,
            "hook_event_name":"SessionStart", "source":"startup"});
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(event.to_string().as_bytes())
            .expect("event");
        let output = child.wait_with_output().expect("helper exit");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("stage=completion"));
        handle.join().expect("HTTP request");
        assert!(installation.read_snapshot("session", &cwd).is_err());

        let recovery = response(listener, serde_json::to_vec(&good).expect("pack"), "200 OK");
        let output = invoke(id, &cwd, adapter, "resume");
        assert!(output.status.success());
        recovery.join().expect("recovery request");
        assert_eq!(
            installation
                .read_snapshot("session", &cwd)
                .expect("fresh Emitted")
                .pack,
            good
        );
    }
}

#[test]
fn blocked_real_stdout_pipe_hits_fixed_deadline_without_late_emission() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::CodexV1, port);
    let mut large = pack(&installation, &cwd);
    large.mandatory[0].content = format!("{} END-MARKER", "x".repeat(10_000));
    let large = GuardrailPack::new(
        large.project,
        large.context,
        large.resolver_schema_version,
        large.mandatory,
    )
    .expect("shared-valid large pack");
    let handle = response(
        listener.try_clone().expect("listener clone"),
        serde_json::to_vec(&large).expect("pack"),
        "200 OK",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("session-start")
        .arg("--installation")
        .arg(root.path().join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(ClientAdapter::CodexV1.name())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("helper");
    let mut stdout = child.stdout.take().expect("stdout pipe");
    // SAFETY: fcntl only changes the size of this owned pipe descriptor.
    let capacity = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
    assert!((0..=4096).contains(&capacity), "pipe capacity: {capacity}");
    let event = serde_json::json!({"session_id":"session", "cwd":cwd,
        "hook_event_name":"SessionStart", "source":"startup"});
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event.to_string().as_bytes())
        .expect("event");
    let began = Instant::now();
    let deadline = began + Duration::from_secs(75);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("cancel stalled helper");
            child.wait().expect("reap helper");
            panic!("helper exceeded its fixed output deadline");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    assert!(began.elapsed() >= Duration::from_secs(55));
    let mut observed = Vec::new();
    stdout.read_to_end(&mut observed).expect("partial stdout");
    assert!(!observed.is_empty());
    assert!(observed.len() <= 4096);
    handle.join().expect("HTTP request");
    assert!(installation.read_snapshot("session", &cwd).is_err());

    let recovery = response(
        listener,
        serde_json::to_vec(&large).expect("pack"),
        "200 OK",
    );
    let output = invoke(id, &cwd, ClientAdapter::CodexV1, "resume");
    assert!(output.status.success());
    recovery.join().expect("recovery request");
    assert_eq!(
        installation
            .read_snapshot("session", &cwd)
            .expect("fresh emission")
            .pack,
        large
    );
}

#[test]
fn unfinished_real_input_times_out_without_guessing_a_session() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::CodexV1, port);
    let good = pack(&installation, &cwd);
    let handle = response(
        listener.try_clone().expect("listener clone"),
        serde_json::to_vec(&good).expect("pack"),
        "200 OK",
    );
    assert!(
        invoke(id, &cwd, ClientAdapter::CodexV1, "startup")
            .status
            .success()
    );
    handle.join().expect("initial request");
    let prior = installation
        .read_snapshot("session", &cwd)
        .expect("prior snapshot")
        .generation;
    let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hooks"))
        .arg("session-start")
        .arg("--installation")
        .arg(root.path().join("control"))
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(ClientAdapter::CodexV1.name())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("helper");
    let mut held_stdin = child.stdin.take().expect("held stdin");
    held_stdin
        .write_all(br#"{"session_id":"session""#)
        .expect("partial event");
    let began = Instant::now();
    let deadline = began + Duration::from_secs(75);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("cancel stalled helper");
            child.wait().expect("reap helper");
            panic!("helper exceeded its fixed input deadline");
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(held_stdin);
    assert!(!status.success());
    assert!(began.elapsed() >= Duration::from_secs(55));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("stdout read");
    assert!(stdout.is_empty());
    assert_eq!(
        installation
            .read_snapshot("session", &cwd)
            .expect("pre-identity failure retains prior generation")
            .generation,
        prior
    );
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    assert!(listener.accept().is_err(), "partial input reached HTTP");
}

#[test]
fn failed_refresh_retires_previous_emission() {
    let root = private_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (installation, id, cwd) = configured(root.path(), ClientAdapter::CodexV1, port);
    let pack = pack(&installation, &cwd);
    let handle = response(
        listener.try_clone().expect("clone listener"),
        serde_json::to_vec(&pack).expect("pack JSON"),
        "200 OK",
    );
    assert!(
        invoke(id, &cwd, ClientAdapter::CodexV1, "startup")
            .status
            .success()
    );
    handle.join().expect("first server");
    installation
        .read_snapshot("session", &cwd)
        .expect("first emission");
    let handle = response(listener, b"{}".to_vec(), "404 Not Found");
    let output = invoke(id, &cwd, ClientAdapter::CodexV1, "resume");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("guardrails_unavailable"));
    assert!(diagnostic.contains("stage=fetch"));
    assert!(diagnostic.contains("generation="));
    assert!(diagnostic.contains("http_status=404"));
    assert!(!diagnostic.contains("fixture-token"));
    assert!(installation.read_snapshot("session", &cwd).is_err());
    handle.join().expect("second server");
}

#[test]
fn upstream_status_is_reported_without_body_or_credential() {
    for (status, code) in [
        ("401 Unauthorized", "401"),
        ("403 Forbidden", "403"),
        ("500 Internal Server Error", "500"),
    ] {
        let root = private_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let (installation, id, cwd) = configured(root.path(), ClientAdapter::CodexV1, port);
        let body = br#"{"error":{"code":"guardrails_transport","message":"SENTINEL_UPSTREAM_BODY","details":{"path":"SENTINEL_PATH"}}}"#.to_vec();
        let handle = response(listener, body, status);
        let output = invoke(id, &cwd, ClientAdapter::CodexV1, "startup");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains(&format!("http_status={code}")));
        assert!(diagnostic.contains("stage=fetch"));
        for secret in ["SENTINEL_UPSTREAM_BODY", "SENTINEL_PATH", "fixture-token"] {
            assert!(!diagnostic.contains(secret));
        }
        assert!(installation.read_snapshot("session", &cwd).is_err());
        handle.join().expect("server");
    }
}
