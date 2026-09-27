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
    invoke_raw_with_env(id, cwd, adapter, event, &[])
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

        let mut wrong_project = good.clone();
        wrong_project.project = "other".to_owned();
        let mut wrong_context = good.clone();
        wrong_context.context.profile = Some("container".to_owned());
        let mut wrong_digest = good.clone();
        wrong_digest.digest = format!("sha256:{}", "0".repeat(64));
        let mut wrong_version = good.clone();
        wrong_version.schema_version = 2;
        let mut empty = good.clone();
        empty.mandatory.clear();
        let cases = [
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
            ("old_daemon", "404 Not Found", b"{}".to_vec()),
            ("unauthorized", "401 Unauthorized", b"{}".to_vec()),
            ("forbidden", "403 Forbidden", b"{}".to_vec()),
            ("server_error", "500 Internal Server Error", b"{}".to_vec()),
        ];
        for (name, status, body) in cases {
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
        let identifiable = [
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"Other", "source":"resume"}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":"refresh"}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":null}),
            serde_json::json!({"session_id":"session", "cwd":cwd,
                "hook_event_name":"SessionStart", "source":"resume", "is_subagent":false}),
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
        for (name, wire) in [
            ("declared_over", declared),
            ("streamed_over", streamed),
            ("truncated", truncated),
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
        let pack = make(low).expect("largest reachable pack");
        assert!(make(low + 1).is_err());
        assert_eq!(pack.mandatory.len(), MAX_POLICIES);
        let canonical = pack.canonical_bytes().expect("canonical bytes");
        let serialized = serde_json::to_vec(&pack).expect("serialized pack");
        let publication = pack.publication().expect("publication");
        let slack = [
            MAX_CANONICAL_BYTES - canonical.len(),
            MAX_PUBLICATION_BYTES - serialized.len(),
            MAX_PUBLICATION_BYTES - publication.len(),
        ];
        assert!(slack.iter().any(|remaining| *remaining < 64), "{slack:?}");
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
