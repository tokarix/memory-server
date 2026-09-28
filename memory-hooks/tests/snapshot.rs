//! Public validated-reader regressions for generation and epoch authority.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use memory_common::guardrails::GuardrailPack;
use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors};
use memory_hooks::snapshot::{PublicationEvent, serialize_publication};
use memory_hooks::{Installation, config::ClientAdapter};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

struct Rig {
    root: TempDir,
    id: Uuid,
    installation: Arc<Installation>,
    config_path: PathBuf,
    cwd: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = tempfile::tempdir_in(base).expect("private fixture");
        let cwd = root.path().join("workspace");
        fs::create_dir(&cwd).expect("workspace");
        let config_path = root.path().join("hooks.toml");
        let id = Installation::initialize(&root.path().join("control"), ClientAdapter::CodexV1)
            .expect("initialize");
        let installation =
            Installation::open(&root.path().join("control"), id, ClientAdapter::CodexV1)
                .expect("open");
        let rig = Self {
            root,
            id,
            installation: Arc::new(installation),
            config_path,
            cwd,
        };
        rig.write_config(&rig.root.path().join("r1"), "workstation");
        rig.installation
            .activate(&rig.config_path)
            .expect("activate");
        rig
    }

    fn write_config(&self, state: &Path, profile: &str) {
        let source = format!(
            "schema_version = 2\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = {profile:?}\n[transport]\nmemoryd_url = \"http://127.0.0.1:55486/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = \"codex-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\nadditional_context_limit = 0\n",
            state.display().to_string(),
            self.cwd.display().to_string()
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.config_path)
            .expect("config");
        file.write_all(source.as_bytes()).expect("write config");
    }

    fn pack(&self, text: &str) -> GuardrailPack {
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
                content: text.to_owned(),
                overrides: None,
            }],
        )
        .expect("valid pack")
    }
}

#[test]
fn exact_output_and_public_reader_require_a_complete_emitted_head() {
    let rig = Rig::new();
    let pending = rig.installation.begin_session("session").expect("pending");
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
    rig.installation
        .attach_binding(&pending, &rig.cwd)
        .expect("attach binding");
    let pack = rig.pack("Rüle \"exact\"\r\n\t🦀 e\u{301}");
    let publication = pack.publication().expect("publication");
    let mut output = Vec::new();
    rig.installation
        .complete_session(&pending, &rig.cwd, pack.clone(), &mut output)
        .expect("emitted");
    assert_eq!(output.last(), Some(&b'\n'));
    let parsed: serde_json::Value = serde_json::from_slice(&output).expect("one JSON response");
    assert_eq!(
        parsed["hookSpecificOutput"]["additionalContext"],
        publication
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    let read = rig
        .installation
        .read_snapshot("session", &rig.cwd)
        .expect("validated reader");
    assert_eq!(read.pack, pack);
    assert_eq!(read.epoch, pending.epoch());
    assert_eq!(read.generation, pending.generation());
}

#[test]
fn child_publication_uses_its_exact_event_envelope() {
    let rig = Rig::new();
    let pack = rig.pack("Child text: 🦀\n\"exact\"");
    let root = serialize_publication(&pack, PublicationEvent::SessionStart).expect("root output");
    let child =
        serialize_publication(&pack, PublicationEvent::SubagentStart).expect("child output");
    assert_ne!(root, child);
    let decoded: serde_json::Value = serde_json::from_slice(&child).expect("child JSON");
    assert_eq!(
        decoded["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
    assert_eq!(
        decoded["hookSpecificOutput"]["additionalContext"],
        pack.publication().expect("publication")
    );
    assert!(child.len() <= memory_hooks::snapshot::OUTPUT_BYTES);
}

struct CrashWriter {
    stage: String,
    barrier: PathBuf,
    accepted_first_byte: bool,
}

impl CrashWriter {
    fn pause(&self) -> ! {
        fs::write(&self.barrier, b"reached").expect("crash barrier");
        loop {
            thread::park();
        }
    }
}

impl Write for CrashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.stage == "prepared" || (self.stage == "partial" && self.accepted_first_byte) {
            self.pause();
        }
        if self.stage == "partial" {
            self.accepted_first_byte = true;
            return Ok(bytes.len().min(1));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.stage == "complete_output" {
            self.pause();
        }
        Ok(())
    }
}

#[test]
fn crash_stage_worker() {
    let Ok(stage) = std::env::var("MEMORY_HOOKS_CRASH_STAGE") else {
        return;
    };
    let control = PathBuf::from(std::env::var_os("MEMORY_HOOKS_CRASH_CONTROL").expect("control"));
    let cwd = PathBuf::from(std::env::var_os("MEMORY_HOOKS_CRASH_CWD").expect("cwd"));
    let barrier = PathBuf::from(std::env::var_os("MEMORY_HOOKS_CRASH_BARRIER").expect("barrier"));
    let id = std::env::var("MEMORY_HOOKS_CRASH_ID")
        .expect("installation id")
        .parse()
        .expect("UUID");
    let installation = Installation::open(&control, id, ClientAdapter::CodexV1).expect("open");
    let pending = installation.begin_session("crash").expect("claim");
    if stage == "pending" {
        fs::write(&barrier, b"reached").expect("barrier");
        loop {
            thread::park();
        }
    }
    installation
        .attach_binding(&pending, &cwd)
        .expect("attach binding");
    if stage == "binding" {
        fs::write(&barrier, b"reached").expect("barrier");
        loop {
            thread::park();
        }
    }
    let binding = installation
        .active()
        .expect("active")
        .config
        .resolve(&cwd)
        .expect("binding");
    let pack = GuardrailPack::new(
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
            content: "crash barrier exact output".to_owned(),
            overrides: None,
        }],
    )
    .expect("pack");
    let mut writer = CrashWriter {
        stage,
        barrier,
        accepted_first_byte: false,
    };
    installation
        .complete_session(&pending, &cwd, pack, &mut writer)
        .expect("worker must pause before completion");
    panic!("worker completed without reaching crash barrier");
}

#[test]
fn terminated_uncommitted_attempts_require_fresh_delivery_after_reopen() {
    for stage in [
        "pending",
        "binding",
        "prepared",
        "partial",
        "complete_output",
    ] {
        let rig = Rig::new();
        let barrier = rig.root.path().join("crash-barrier");
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "crash_stage_worker"])
            .env("MEMORY_HOOKS_CRASH_STAGE", stage)
            .env(
                "MEMORY_HOOKS_CRASH_CONTROL",
                rig.root.path().join("control"),
            )
            .env("MEMORY_HOOKS_CRASH_CWD", &rig.cwd)
            .env("MEMORY_HOOKS_CRASH_BARRIER", &barrier)
            .env("MEMORY_HOOKS_CRASH_ID", rig.id.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("worker process");
        let deadline = Instant::now() + Duration::from_secs(8);
        while !barrier.exists() && Instant::now() < deadline {
            assert!(
                child.try_wait().expect("worker status").is_none(),
                "{stage}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(barrier.exists(), "worker did not reach {stage}");
        child.kill().expect("terminate worker");
        child.wait().expect("reap worker");
        let reopened = Installation::open(
            &rig.root.path().join("control"),
            rig.id,
            ClientAdapter::CodexV1,
        )
        .expect("reopen installation");
        assert!(
            reopened.read_snapshot("crash", &rig.cwd).is_err(),
            "{stage}"
        );
        let fresh = reopened.begin_session("crash").expect("fresh generation");
        reopened
            .attach_binding(&fresh, &rig.cwd)
            .expect("fresh binding");
        let pack = rig.pack("fresh output after crash");
        let mut output = Vec::new();
        reopened
            .complete_session(&fresh, &rig.cwd, pack.clone(), &mut output)
            .expect("fresh emission");
        assert_eq!(output.last(), Some(&b'\n'));
        assert_eq!(
            reopened
                .read_snapshot("crash", &rig.cwd)
                .expect("current reader")
                .pack,
            pack
        );
    }
}

#[test]
fn unresolved_head_requires_explicit_sequence_advancing_repair() {
    let rig = Rig::new();
    let pending = rig
        .installation
        .begin_session("repair-me")
        .expect("pending");
    rig.installation
        .attach_binding(&pending, &rig.cwd)
        .expect("binding");
    let pack = rig.pack("old text must not return after repair");
    rig.installation
        .complete_session(&pending, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("first emission");
    let control = rig.root.path().join("control");
    let journal = fs::read_dir(control)
        .expect("anchor entries")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("journal-"))
        })
        .expect("session journal");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).expect("journal read")).expect("journal JSON");
    value["resolved"] = serde_json::Value::Bool(false);
    fs::write(
        &journal,
        serde_json::to_vec(&value).expect("journal encoding"),
    )
    .expect("incomplete transition");
    assert!(
        rig.installation
            .read_snapshot("repair-me", &rig.cwd)
            .is_err()
    );
    assert!(rig.installation.begin_session("repair-me").is_err());
    assert_eq!(
        rig.installation
            .repair_session("repair-me")
            .expect("repair"),
        2
    );
    assert!(
        rig.installation
            .read_snapshot("repair-me", &rig.cwd)
            .is_err()
    );
    let fresh = rig.installation.begin_session("repair-me").expect("fresh");
    rig.installation
        .attach_binding(&fresh, &rig.cwd)
        .expect("fresh binding");
    rig.installation
        .complete_session(&fresh, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("fresh emission");
    assert_eq!(
        rig.installation
            .read_snapshot("repair-me", &rig.cwd)
            .expect("current reader")
            .pack,
        pack
    );
    assert_ne!(pending.generation(), fresh.generation());
}

#[test]
fn session_claim_and_repair_reject_sequence_overflow_without_reusing_payload() {
    for unresolved in [false, true] {
        let rig = Rig::new();
        let first = rig
            .installation
            .begin_session("overflow")
            .expect("first claim");
        rig.installation
            .attach_binding(&first, &rig.cwd)
            .expect("first binding");
        rig.installation
            .complete_session(&first, &rig.cwd, rig.pack("old"), &mut Vec::new())
            .expect("first emission");
        let control = rig.root.path().join("control");
        let head = fs::read_dir(&control)
            .expect("control entries")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("head-"))
            })
            .expect("head");
        let journal = fs::read_dir(&control)
            .expect("control entries")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("journal-"))
            })
            .expect("journal");
        let mut head_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&head).expect("head bytes")).expect("head JSON");
        let mut journal_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&journal).expect("journal bytes"))
                .expect("journal JSON");
        head_value["sequence"] = serde_json::json!(u64::MAX);
        journal_value["sequence"] = serde_json::json!(u64::MAX);
        journal_value["resolved"] = serde_json::json!(!unresolved);
        fs::write(
            &head,
            serde_json::to_vec(&head_value).expect("head encoding"),
        )
        .expect("tamper head");
        fs::write(
            &journal,
            serde_json::to_vec(&journal_value).expect("journal encoding"),
        )
        .expect("tamper journal");
        let reopened = Installation::open(&control, rig.id, ClientAdapter::CodexV1)
            .expect("reopen installation");
        assert!(reopened.read_snapshot("overflow", &rig.cwd).is_err());
        assert_eq!(
            reopened
                .begin_session("overflow")
                .err()
                .expect("claim must fail")
                .code(),
            if unresolved {
                "snapshot_invalid"
            } else {
                "session_invalid"
            }
        );
        assert!(reopened.repair_session("overflow").is_err());
        let separate = reopened.begin_session("separate").expect("separate claim");
        reopened
            .attach_binding(&separate, &rig.cwd)
            .expect("separate binding");
        let current = rig.pack("separate exact");
        let mut output = Vec::new();
        reopened
            .complete_session(&separate, &rig.cwd, current.clone(), &mut output)
            .expect("separate emission");
        assert_eq!(output.last(), Some(&b'\n'));
        assert_eq!(
            reopened
                .read_snapshot("separate", &rig.cwd)
                .expect("separate reader")
                .pack,
            current
        );
        assert!(reopened.read_snapshot("overflow", &rig.cwd).is_err());
    }
}

#[test]
fn missing_corrupt_and_oversized_payloads_never_reuse_emitted_evidence() {
    for damage in ["missing", "corrupt", "oversized", "inner_version"] {
        let rig = Rig::new();
        let first = rig.installation.begin_session("payload").expect("first");
        rig.installation
            .attach_binding(&first, &rig.cwd)
            .expect("first binding");
        let pack = rig.pack("exact before and after damage");
        rig.installation
            .complete_session(&first, &rig.cwd, pack.clone(), &mut Vec::new())
            .expect("first emission");
        let payload = fs::read_dir(rig.root.path().join("r1"))
            .expect("payload root")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("payload-"))
            })
            .expect("payload record");
        match damage {
            "missing" => fs::remove_file(&payload).expect("remove payload"),
            "corrupt" => fs::write(&payload, b"not JSON").expect("corrupt payload"),
            "oversized" => fs::write(&payload, vec![b'x'; memory_hooks::limits::RECORD_BYTES + 1])
                .expect("oversize payload"),
            "inner_version" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&payload).expect("payload bytes"))
                        .expect("payload JSON");
                value["publication_version"] = serde_json::Value::from(2);
                let bytes = serde_json::to_vec(&value).expect("payload encoding");
                fs::write(&payload, &bytes).expect("replace payload");
                let head = fs::read_dir(rig.root.path().join("control"))
                    .expect("control root")
                    .map(|entry| entry.expect("entry").path())
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("head-"))
                    })
                    .expect("head record");
                let mut head_value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&head).expect("head bytes"))
                        .expect("head JSON");
                head_value["payload_sha256"] =
                    serde_json::Value::from(format!("{:x}", Sha256::digest(&bytes)));
                fs::write(
                    &head,
                    serde_json::to_vec(&head_value).expect("head encoding"),
                )
                .expect("replace head");
            }
            _ => unreachable!(),
        }
        let reopened = Installation::open(
            &rig.root.path().join("control"),
            rig.id,
            ClientAdapter::CodexV1,
        )
        .expect("reopen installation");
        assert!(
            reopened.read_snapshot("payload", &rig.cwd).is_err(),
            "{damage}"
        );
        let fresh = reopened.begin_session("payload").expect("fresh claim");
        assert_ne!(fresh.generation(), first.generation());
        reopened
            .attach_binding(&fresh, &rig.cwd)
            .expect("fresh binding");
        let mut output = Vec::new();
        reopened
            .complete_session(&fresh, &rig.cwd, pack.clone(), &mut output)
            .expect("fresh emission");
        assert_eq!(
            reopened
                .read_snapshot("payload", &rig.cwd)
                .expect("fresh reader")
                .pack,
            pack
        );
        assert_eq!(output.last(), Some(&b'\n'));
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "each independently altered payload field shares the same reopened-reader and recovery assertion"
)]
fn inner_payload_scope_and_output_tampering_fail_even_with_a_matching_outer_hash() {
    for field in [
        "version",
        "installation_id",
        "client",
        "session_hash",
        "epoch",
        "nonce",
        "generation",
        "sequence",
        "binding",
        "publication_version",
        "output_sha256",
        "output_bytes",
        "pack_project",
        "pack_context",
        "pack_digest",
        "pack_content",
    ] {
        let rig = Rig::new();
        let first = rig
            .installation
            .begin_session("payload")
            .expect("first claim");
        rig.installation
            .attach_binding(&first, &rig.cwd)
            .expect("first binding");
        let pack = rig.pack("exact content");
        rig.installation
            .complete_session(&first, &rig.cwd, pack.clone(), &mut Vec::new())
            .expect("first emission");
        let payload = fs::read_dir(rig.root.path().join("r1"))
            .expect("payload root")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("payload-"))
            })
            .expect("payload");
        let head = fs::read_dir(rig.root.path().join("control"))
            .expect("control root")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("head-"))
            })
            .expect("head");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&payload).expect("payload bytes"))
                .expect("payload JSON");
        match field {
            "version" => value["version"] = serde_json::json!(2),
            "installation_id" => value["installation_id"] = serde_json::json!(Uuid::new_v4()),
            "client" => value["client"] = serde_json::json!("claude-v1"),
            "session_hash" => value["session_hash"] = serde_json::json!("wrong"),
            "epoch" => value["epoch"] = serde_json::json!(first.epoch() + 1),
            "nonce" => value["nonce"] = serde_json::json!(Uuid::new_v4()),
            "generation" => value["generation"] = serde_json::json!(Uuid::new_v4()),
            "sequence" => value["sequence"] = serde_json::json!(2),
            "binding" => value["binding"] = serde_json::json!("wrong binding"),
            "publication_version" => value["publication_version"] = serde_json::json!(2),
            "output_sha256" => value["output_sha256"] = serde_json::json!("wrong"),
            "output_bytes" => value["output_bytes"] = serde_json::json!(1),
            "pack_project" => value["pack"]["project"] = serde_json::json!("other"),
            "pack_context" => {
                value["pack"]["context"]["profile"] = serde_json::json!("container");
            }
            "pack_digest" => value["pack"]["digest"] = serde_json::json!("wrong"),
            "pack_content" => {
                value["pack"]["mandatory"][0]["content"] = serde_json::json!("changed content");
            }
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&value).expect("payload encoding");
        fs::write(&payload, &bytes).expect("tamper payload");
        let mut head_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&head).expect("head bytes")).expect("head JSON");
        head_value["payload_sha256"] =
            serde_json::json!(format!("sha256:{:x}", Sha256::digest(&bytes)));
        fs::write(
            &head,
            serde_json::to_vec(&head_value).expect("head encoding"),
        )
        .expect("update outer hash");
        let reopened = Installation::open(
            &rig.root.path().join("control"),
            rig.id,
            ClientAdapter::CodexV1,
        )
        .expect("reopen installation");
        assert!(
            reopened.read_snapshot("payload", &rig.cwd).is_err(),
            "{field}"
        );
        let fresh = reopened.begin_session("payload").expect("fresh claim");
        assert_ne!(fresh.generation(), first.generation());
        reopened
            .attach_binding(&fresh, &rig.cwd)
            .expect("fresh binding");
        let mut output = Vec::new();
        reopened
            .complete_session(&fresh, &rig.cwd, pack.clone(), &mut output)
            .expect("fresh emission");
        assert_eq!(output.last(), Some(&b'\n'), "{field}");
        assert_eq!(
            reopened
                .read_snapshot("payload", &rig.cwd)
                .expect("fresh reader")
                .pack,
            pack,
            "{field}"
        );
    }
}

#[test]
fn absent_corrupt_and_oversized_heads_cannot_fall_back_to_old_payloads() {
    for damage in ["missing", "corrupt", "oversized", "unknown_version"] {
        let rig = Rig::new();
        let first = rig.installation.begin_session("head").expect("first");
        rig.installation
            .attach_binding(&first, &rig.cwd)
            .expect("binding");
        rig.installation
            .complete_session(&first, &rig.cwd, rig.pack("old"), &mut Vec::new())
            .expect("emission");
        let head = fs::read_dir(rig.root.path().join("control"))
            .expect("control root")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("head-"))
            })
            .expect("head record");
        match damage {
            "missing" => fs::remove_file(&head).expect("remove head"),
            "corrupt" => fs::write(&head, b"not JSON").expect("corrupt head"),
            "oversized" => fs::write(&head, vec![b'x'; memory_hooks::limits::RECORD_BYTES + 1])
                .expect("oversize head"),
            "unknown_version" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&head).expect("head bytes"))
                        .expect("head JSON");
                value["version"] = serde_json::Value::from(2);
                fs::write(&head, serde_json::to_vec(&value).expect("head encoding"))
                    .expect("replace head");
            }
            _ => unreachable!(),
        }
        let reopened = Installation::open(
            &rig.root.path().join("control"),
            rig.id,
            ClientAdapter::CodexV1,
        )
        .expect("reopen installation");
        assert!(
            reopened.read_snapshot("head", &rig.cwd).is_err(),
            "{damage}"
        );
        assert!(reopened.begin_session("head").is_err(), "{damage}");
        let separate = reopened.begin_session("separate").expect("separate claim");
        reopened
            .attach_binding(&separate, &rig.cwd)
            .expect("separate binding");
        let pack = rig.pack("separate session");
        reopened
            .complete_session(&separate, &rig.cwd, pack.clone(), &mut Vec::new())
            .expect("separate emission");
        assert_eq!(
            reopened
                .read_snapshot("separate", &rig.cwd)
                .expect("separate reader")
                .pack,
            pack
        );
        assert!(reopened.read_snapshot("head", &rig.cwd).is_err());
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "each independent head or journal mutation shares the same reopened-reader and isolation assertion"
)]
fn head_and_journal_fields_independently_gate_reader_authority() {
    for field in [
        "head_version",
        "installation_id",
        "client",
        "session_hash",
        "epoch",
        "nonce",
        "generation",
        "sequence",
        "state",
        "binding",
        "payload_name",
        "payload_sha256",
        "output_sha256",
        "output_bytes",
        "journal_version",
        "journal_generation",
        "journal_sequence",
        "journal_resolved",
        "journal_missing",
    ] {
        let rig = Rig::new();
        let first = rig.installation.begin_session("head").expect("claim");
        rig.installation
            .attach_binding(&first, &rig.cwd)
            .expect("binding");
        rig.installation
            .complete_session(&first, &rig.cwd, rig.pack("exact"), &mut Vec::new())
            .expect("emission");
        let control = rig.root.path().join("control");
        let head = fs::read_dir(&control)
            .expect("control entries")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("head-"))
            })
            .expect("head record");
        let journal = fs::read_dir(&control)
            .expect("control entries")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("journal-"))
            })
            .expect("journal record");
        let mut head_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&head).expect("head bytes")).expect("head JSON");
        let mut journal_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&journal).expect("journal bytes"))
                .expect("journal JSON");
        match field {
            "head_version" => head_value["version"] = serde_json::json!(2),
            "installation_id" => {
                head_value["installation_id"] = serde_json::json!(Uuid::new_v4());
            }
            "client" => head_value["client"] = serde_json::json!("claude-v1"),
            "session_hash" => head_value["session_hash"] = serde_json::json!("wrong"),
            "epoch" => head_value["epoch"] = serde_json::json!(first.epoch() + 1),
            "nonce" => head_value["nonce"] = serde_json::json!(Uuid::new_v4()),
            "generation" => {
                let changed = serde_json::json!(Uuid::new_v4());
                head_value["generation"] = changed.clone();
                journal_value["generation"] = changed;
            }
            "sequence" => {
                head_value["sequence"] = serde_json::json!(2);
                journal_value["sequence"] = serde_json::json!(2);
            }
            "state" => head_value["state"] = serde_json::json!("prepared"),
            "binding" => head_value["binding"] = serde_json::json!("wrong"),
            "payload_name" => head_value["payload_name"] = serde_json::json!("payload-wrong"),
            "payload_sha256" => head_value["payload_sha256"] = serde_json::json!("wrong"),
            "output_sha256" => head_value["output_sha256"] = serde_json::json!("wrong"),
            "output_bytes" => head_value["output_bytes"] = serde_json::json!(1),
            "journal_version" => journal_value["version"] = serde_json::json!(2),
            "journal_generation" => journal_value["generation"] = serde_json::json!(Uuid::new_v4()),
            "journal_sequence" => journal_value["sequence"] = serde_json::json!(2),
            "journal_resolved" => journal_value["resolved"] = serde_json::json!(false),
            "journal_missing" => fs::remove_file(&journal).expect("remove journal"),
            _ => unreachable!(),
        }
        fs::write(
            &head,
            serde_json::to_vec(&head_value).expect("head encoding"),
        )
        .expect("tamper head");
        if field != "journal_missing" {
            fs::write(
                &journal,
                serde_json::to_vec(&journal_value).expect("journal encoding"),
            )
            .expect("tamper journal");
        }
        let reopened = Installation::open(&control, rig.id, ClientAdapter::CodexV1)
            .expect("reopen installation");
        assert!(reopened.read_snapshot("head", &rig.cwd).is_err(), "{field}");
        let other = reopened
            .begin_session("separate")
            .expect("independent claim");
        reopened
            .attach_binding(&other, &rig.cwd)
            .expect("independent binding");
        let pack = rig.pack("independent session");
        let mut output = Vec::new();
        reopened
            .complete_session(&other, &rig.cwd, pack.clone(), &mut output)
            .expect("independent emission");
        assert_eq!(output.last(), Some(&b'\n'), "{field}");
        assert_eq!(
            reopened
                .read_snapshot("separate", &rig.cwd)
                .expect("independent reader")
                .pack,
            pack,
            "{field}"
        );
        assert!(reopened.read_snapshot("head", &rig.cwd).is_err(), "{field}");
    }
}

#[test]
fn newer_failure_and_root_reversion_never_restore_an_old_head() {
    let rig = Rig::new();
    let a = rig.installation.begin_session("S").expect("A");
    rig.installation
        .attach_binding(&a, &rig.cwd)
        .expect("A binding");
    let pack = rig.pack("same digest after restoration");
    rig.installation
        .complete_session(&a, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("A emitted");
    rig.installation
        .read_snapshot("S", &rig.cwd)
        .expect("A current");

    let b = rig.installation.begin_session("S").expect("B");
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    let stale = rig
        .installation
        .complete_session(&a, &rig.cwd, pack.clone(), &mut Vec::new());
    assert_eq!(stale.expect_err("A stale").code(), "session_stale");
    rig.installation
        .attach_binding(&b, &rig.cwd)
        .expect("B binding");
    let mut wrong = pack.clone();
    wrong.project = "wrong".to_owned();
    assert!(
        rig.installation
            .complete_session(&b, &rig.cwd, wrong, &mut Vec::new())
            .is_err()
    );
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());

    rig.write_config(&rig.root.path().join("r2"), "workstation");
    assert_eq!(rig.installation.activate(&rig.config_path).expect("R2"), 2);
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    rig.write_config(&rig.root.path().join("r1"), "workstation");
    assert_eq!(rig.installation.activate(&rig.config_path).expect("R1"), 3);
    assert!(rig.root.path().join("r1").is_dir());
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    let c = rig.installation.begin_session("S").expect("C");
    rig.installation
        .attach_binding(&c, &rig.cwd)
        .expect("C binding");
    rig.installation
        .complete_session(&c, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("C emitted");
    assert_eq!(
        rig.installation
            .read_snapshot("S", &rig.cwd)
            .expect("C current")
            .pack,
        pack
    );
    assert_ne!(a.generation(), c.generation());
}

struct FailAfter {
    limit: usize,
    written: Vec<u8>,
    flush_error: bool,
}

impl Write for FailAfter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written.len() >= self.limit {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture"));
        }
        let count = bytes.len().min(self.limit - self.written.len());
        self.written.extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_error {
            Err(io::Error::other("fixture flush"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn partial_output_and_flush_failure_leave_prepared_unusable() {
    for (limit, flush_error) in [(0, false), (1, false), (27, false), (usize::MAX, true)] {
        let rig = Rig::new();
        let pending = rig.installation.begin_session("S").expect("pending");
        rig.installation
            .attach_binding(&pending, &rig.cwd)
            .expect("binding");
        let mut writer = FailAfter {
            limit,
            written: Vec::new(),
            flush_error,
        };
        assert!(
            rig.installation
                .complete_session(&pending, &rig.cwd, rig.pack("exact"), &mut writer)
                .is_err()
        );
        assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
        let refresh = rig.installation.begin_session("S").expect("new generation");
        assert_ne!(pending.generation(), refresh.generation());
    }
}

#[test]
fn zero_byte_writer_cannot_commit_emitted() {
    struct ZeroWriter;
    impl Write for ZeroWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let rig = Rig::new();
    let pending = rig.installation.begin_session("S").expect("pending");
    rig.installation
        .attach_binding(&pending, &rig.cwd)
        .expect("binding");
    assert!(
        rig.installation
            .complete_session(&pending, &rig.cwd, rig.pack("exact"), &mut ZeroWriter)
            .is_err()
    );
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
}

#[test]
fn interrupted_short_writes_commit_exactly_and_last_byte_failures_do_not() {
    struct ShortWriter {
        bytes: Vec<u8>,
        calls: usize,
        flushed: bool,
    }

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls % 3 == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed = true;
            Ok(())
        }
    }

    let rig = Rig::new();
    let pack = rig.pack("exact output to final newline END-MARKER");
    let pending = rig.installation.begin_session("S").expect("pending");
    rig.installation
        .attach_binding(&pending, &rig.cwd)
        .expect("binding");
    let mut short = ShortWriter {
        bytes: Vec::new(),
        calls: 0,
        flushed: false,
    };
    rig.installation
        .complete_session(&pending, &rig.cwd, pack.clone(), &mut short)
        .expect("short writes complete");
    assert!(short.calls > 3);
    assert!(short.flushed);
    assert_eq!(short.bytes.last(), Some(&b'\n'));
    let decoded: serde_json::Value =
        serde_json::from_slice(&short.bytes).expect("one complete JSON object");
    assert_eq!(
        decoded["hookSpecificOutput"]["additionalContext"],
        pack.publication().expect("publication")
    );
    assert_eq!(
        rig.installation
            .read_snapshot("S", &rig.cwd)
            .expect("public reader")
            .pack,
        pack
    );

    for cutoff in [short.bytes.len() - 2, short.bytes.len() - 1] {
        let pending = rig.installation.begin_session("S").expect("fresh pending");
        rig.installation
            .attach_binding(&pending, &rig.cwd)
            .expect("binding");
        let mut writer = FailAfter {
            limit: cutoff,
            written: Vec::new(),
            flush_error: false,
        };
        assert!(
            rig.installation
                .complete_session(&pending, &rig.cwd, pack.clone(), &mut writer)
                .is_err()
        );
        assert_eq!(writer.written, short.bytes[..cutoff]);
        assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    }
    let recovered = rig.installation.begin_session("S").expect("recovery");
    rig.installation
        .attach_binding(&recovered, &rig.cwd)
        .expect("binding");
    rig.installation
        .complete_session(&recovered, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("fresh complete delivery");
    assert_eq!(
        rig.installation
            .read_snapshot("S", &rig.cwd)
            .expect("recovered reader")
            .pack,
        pack
    );
}

#[test]
fn migration_wins_over_a_delayed_old_completion() {
    let rig = Rig::new();
    let a = rig.installation.begin_session("S").expect("A");
    rig.installation
        .attach_binding(&a, &rig.cwd)
        .expect("A binding");
    let old_pack = rig.pack("A exact");
    let installation = Arc::clone(&rig.installation);
    let cwd = rig.cwd.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let delayed = thread::spawn(move || {
        ready_tx.send(()).expect("ready");
        release_rx.recv().expect("release");
        let mut output = Vec::new();
        let result = installation.complete_session(&a, &cwd, old_pack, &mut output);
        (result, output)
    });
    ready_rx.recv().expect("waiting fetch");
    rig.write_config(&rig.root.path().join("r2"), "workstation");
    rig.installation
        .activate(&rig.config_path)
        .expect("migration");
    let b = rig.installation.begin_session("S").expect("B");
    rig.installation
        .attach_binding(&b, &rig.cwd)
        .expect("B binding");
    let b_pack = rig.pack("B exact");
    rig.installation
        .complete_session(&b, &rig.cwd, b_pack.clone(), &mut Vec::new())
        .expect("B emitted");
    release_tx.send(()).expect("release A");
    let (result, output) = delayed.join().expect("A join");
    assert_eq!(result.expect_err("A stale").code(), "session_stale");
    assert!(output.is_empty());
    assert_eq!(
        rig.installation
            .read_snapshot("S", &rig.cwd)
            .expect("B current")
            .pack,
        b_pack
    );
}

struct PauseWriter {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    bytes: Vec<u8>,
}

impl Write for PauseWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.entered.send(()).expect("writer entered");
        self.release.recv().expect("writer released");
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn emission_under_fixed_lock_is_retired_by_following_root_activation() {
    let rig = Rig::new();
    let pending = rig.installation.begin_session("S").expect("pending");
    rig.installation
        .attach_binding(&pending, &rig.cwd)
        .expect("binding");
    let pack = rig.pack("exact before migration");
    let old_root = rig.root.path().join("r1");
    let next_root = rig.root.path().join("r2");
    let next_config = rig.root.path().join("next.toml");
    let source = fs::read_to_string(&rig.config_path).expect("current config");
    let old = format!("{:?}", old_root.display().to_string());
    let next = format!("{:?}", next_root.display().to_string());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&next_config)
        .expect("new config");
    file.write_all(source.replace(&old, &next).as_bytes())
        .expect("new config bytes");

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let installation = Arc::clone(&rig.installation);
    let cwd = rig.cwd.clone();
    let writer = thread::spawn(move || {
        let mut output = PauseWriter {
            entered: entered_tx,
            release: release_rx,
            bytes: Vec::new(),
        };
        let result = installation.complete_session(&pending, &cwd, pack, &mut output);
        (result, output.bytes)
    });
    entered_rx.recv().expect("writer held installation lock");
    let installation = Arc::clone(&rig.installation);
    let (activation_tx, activation_rx) = mpsc::channel();
    let activation = thread::spawn(move || {
        activation_tx.send(()).expect("activation started");
        installation.activate(&next_config)
    });
    activation_rx.recv().expect("activation queued");
    release_tx.send(()).expect("finish output");
    let (result, output) = writer.join().expect("writer join");
    result.expect("emission before activation barrier");
    assert!(!output.is_empty());
    assert_eq!(activation.join().expect("activation join").expect("R2"), 2);
    assert!(old_root.is_dir(), "old payload root remains on disk");
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    let fresh = rig.installation.begin_session("S").expect("fresh R2 start");
    rig.installation
        .attach_binding(&fresh, &rig.cwd)
        .expect("R2 binding");
    let current = rig.pack("exact after migration");
    rig.installation
        .complete_session(&fresh, &rig.cwd, current.clone(), &mut Vec::new())
        .expect("R2 emission");
    assert_eq!(
        rig.installation
            .read_snapshot("S", &rig.cwd)
            .expect("R2 reader")
            .pack,
        current
    );
}

#[test]
fn root_reversion_retires_sessions_that_never_refreshed() {
    let rig = Rig::new();
    let pack = rig.pack("same pack on every root");
    for session in ["refreshed", "idle"] {
        let pending = rig.installation.begin_session(session).expect("R1 pending");
        rig.installation
            .attach_binding(&pending, &rig.cwd)
            .expect("R1 binding");
        rig.installation
            .complete_session(&pending, &rig.cwd, pack.clone(), &mut Vec::new())
            .expect("R1 emission");
        rig.installation
            .read_snapshot(session, &rig.cwd)
            .expect("R1 reader");
    }
    rig.write_config(&rig.root.path().join("r2"), "workstation");
    rig.installation.activate(&rig.config_path).expect("R2");
    let current = rig
        .installation
        .begin_session("refreshed")
        .expect("R2 pending");
    rig.installation
        .attach_binding(&current, &rig.cwd)
        .expect("R2 binding");
    rig.installation
        .complete_session(&current, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("R2 emission");
    assert!(rig.installation.read_snapshot("idle", &rig.cwd).is_err());
    rig.write_config(&rig.root.path().join("r1"), "workstation");
    rig.installation
        .activate(&rig.config_path)
        .expect("R1 again");
    for session in ["refreshed", "idle"] {
        assert!(rig.installation.read_snapshot(session, &rig.cwd).is_err());
    }
    assert!(rig.root.path().join("r1").is_dir());
    assert!(rig.root.path().join("r2").is_dir());
}

#[test]
fn distinct_supported_filesystems_preserve_epoch_retirement_across_root_reversion() {
    let Some(secondary_base) = std::env::var_os("MEMORY_HOOKS_TEST_ROOT_SECONDARY") else {
        eprintln!(
            "distinct-filesystem fixture unavailable: MEMORY_HOOKS_TEST_ROOT_SECONDARY unset"
        );
        return;
    };
    let rig = Rig::new();
    let secondary = tempfile::tempdir_in(secondary_base).expect("second private fixture");
    assert_ne!(
        fs::metadata(rig.root.path())
            .expect("primary metadata")
            .dev(),
        fs::metadata(secondary.path())
            .expect("secondary metadata")
            .dev(),
        "fixture roots must be on distinct filesystems"
    );
    let pack = rig.pack("exact pack across independent filesystems");
    for session in ["refreshed", "idle"] {
        let pending = rig.installation.begin_session(session).expect("R1 claim");
        rig.installation
            .attach_binding(&pending, &rig.cwd)
            .expect("R1 binding");
        rig.installation
            .complete_session(&pending, &rig.cwd, pack.clone(), &mut Vec::new())
            .expect("R1 delivery");
    }
    let first = rig
        .installation
        .read_snapshot("refreshed", &rig.cwd)
        .expect("R1 reader");
    let primary_root = rig.root.path().join("r1");
    rig.write_config(&secondary.path().join("r2"), "workstation");
    rig.installation
        .activate(&rig.config_path)
        .expect("R2 activation");
    for session in ["refreshed", "idle"] {
        assert!(rig.installation.read_snapshot(session, &rig.cwd).is_err());
    }
    let second = rig
        .installation
        .begin_session("refreshed")
        .expect("R2 claim");
    rig.installation
        .attach_binding(&second, &rig.cwd)
        .expect("R2 binding");
    rig.installation
        .complete_session(&second, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("R2 delivery");
    assert_eq!(
        rig.installation
            .read_snapshot("refreshed", &rig.cwd)
            .expect("R2 reader")
            .pack,
        pack
    );
    rig.write_config(&primary_root, "workstation");
    rig.installation
        .activate(&rig.config_path)
        .expect("R1 reactivation");
    for session in ["refreshed", "idle"] {
        assert!(rig.installation.read_snapshot(session, &rig.cwd).is_err());
    }
    let fresh = rig
        .installation
        .begin_session("refreshed")
        .expect("fresh claim");
    assert_ne!(fresh.epoch(), first.epoch);
    assert_ne!(fresh.generation(), first.generation);
    rig.installation
        .attach_binding(&fresh, &rig.cwd)
        .expect("fresh binding");
    rig.installation
        .complete_session(&fresh, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("fresh delivery");
    assert_eq!(
        rig.installation
            .read_snapshot("refreshed", &rig.cwd)
            .expect("fresh reader")
            .pack,
        pack
    );
}

#[test]
fn failed_output_on_intermediate_root_cannot_restore_first_root_evidence() {
    let rig = Rig::new();
    let pack = rig.pack("identical policy across all epochs");
    let first = rig.installation.begin_session("S").expect("R1 claim");
    rig.installation
        .attach_binding(&first, &rig.cwd)
        .expect("R1 binding");
    rig.installation
        .complete_session(&first, &rig.cwd, pack.clone(), &mut Vec::new())
        .expect("R1 emission");
    let first_read = rig
        .installation
        .read_snapshot("S", &rig.cwd)
        .expect("R1 reader");

    rig.write_config(&rig.root.path().join("r2"), "workstation");
    assert_eq!(rig.installation.activate(&rig.config_path).expect("R2"), 2);
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());
    let second = rig.installation.begin_session("S").expect("R2 claim");
    rig.installation
        .attach_binding(&second, &rig.cwd)
        .expect("R2 binding");
    let mut broken = FailAfter {
        limit: 1,
        written: Vec::new(),
        flush_error: false,
    };
    assert!(
        rig.installation
            .complete_session(&second, &rig.cwd, pack.clone(), &mut broken)
            .is_err()
    );
    assert_eq!(broken.written.len(), 1);
    assert!(rig.installation.read_snapshot("S", &rig.cwd).is_err());

    rig.write_config(&rig.root.path().join("r1"), "workstation");
    assert_eq!(
        rig.installation
            .activate(&rig.config_path)
            .expect("R1 again"),
        3
    );
    let reopened = Installation::open(
        &rig.root.path().join("control"),
        rig.id,
        ClientAdapter::CodexV1,
    )
    .expect("reopen anchor");
    assert!(reopened.read_snapshot("S", &rig.cwd).is_err());
    let fresh = reopened.begin_session("S").expect("R1 fresh claim");
    assert_ne!(fresh.epoch(), first_read.epoch);
    assert_ne!(fresh.generation(), first_read.generation);
    reopened
        .attach_binding(&fresh, &rig.cwd)
        .expect("fresh binding");
    let mut output = Vec::new();
    reopened
        .complete_session(&fresh, &rig.cwd, pack.clone(), &mut output)
        .expect("fresh emission");
    assert_eq!(output.last(), Some(&b'\n'));
    assert_eq!(
        reopened
            .read_snapshot("S", &rig.cwd)
            .expect("fresh reader")
            .pack,
        pack
    );
}
