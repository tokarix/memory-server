//! Subprocess contract for fresh client-consumed mandatory startup output.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use memory_common::guardrails::GuardrailPack;
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

fn response(listener: TcpListener, body: Vec<u8>, status: &str) -> thread::JoinHandle<String> {
    let status = status.to_owned();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
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
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).expect("headers");
        stream.write_all(&body).expect("body");
        String::from_utf8(request).expect("request UTF-8")
    })
}

fn invoke(id: Uuid, cwd: &Path, adapter: ClientAdapter, source: &str) -> std::process::Output {
    let event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "session",
        "cwd": cwd,
        "source": source,
        "agent_type": "custom-top-level"
    });
    invoke_raw(id, cwd, adapter, event.to_string().as_bytes())
}

fn invoke_raw(id: Uuid, cwd: &Path, adapter: ClientAdapter, event: &[u8]) -> std::process::Output {
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
        .spawn()
        .expect("spawn helper");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(event)
        .expect("write event");
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("guardrails_unavailable"));
    assert!(installation.read_snapshot("session", &cwd).is_err());
    handle.join().expect("second server");
}
