//! Fixed-anchor activation and retirement contracts.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use memory_hooks::{Installation, Lifecycle, config::ClientAdapter};
use tempfile::TempDir;
use uuid::Uuid;

fn fixture() -> TempDir {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from);
    tempfile::tempdir_in(base).expect("private fixture")
}

fn write_config(base: &Path, state: &Path, profile: &str) -> PathBuf {
    let path = base.join("hooks.toml");
    let source = format!(
        "schema_version = 2\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = {profile:?}\n[transport]\nmemoryd_url = \"http://127.0.0.1:55486/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = \"codex-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\nadditional_context_limit = 0\n",
        state.display().to_string(),
        base.join("workspace").display().to_string()
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .expect("config");
    file.write_all(source.as_bytes()).expect("write config");
    path
}

#[test]
fn activation_reversion_uses_fresh_epoch_and_retirement_is_permanent() {
    let root = fixture();
    fs::create_dir(root.path().join("workspace")).expect("workspace");
    let anchor = root.path().join("control");
    let r1 = root.path().join("r1");
    let r2 = root.path().join("r2");
    let config_path = write_config(root.path(), &r1, "workstation");
    assert!(Installation::open(&anchor, Uuid::new_v4(), ClientAdapter::CodexV1).is_err());
    let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
    assert!(Installation::initialize(&anchor, ClientAdapter::CodexV1).is_err());
    assert!(Installation::open(&anchor, Uuid::new_v4(), ClientAdapter::CodexV1).is_err());
    assert!(Installation::open(&anchor, id, ClientAdapter::ClaudeV1).is_err());
    let installation = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
    assert_eq!(
        installation.status().expect("empty status"),
        (Lifecycle::Changing, 0)
    );

    assert_eq!(installation.activate(&config_path).expect("activate R1"), 1);
    let a = installation.active().expect("active R1");
    assert_eq!(a.epoch, 1);
    assert_eq!(a.config.fingerprint().get(..3), Some("v2:"));
    assert!(r1.is_dir());

    write_config(root.path(), &r2, "workstation");
    assert_eq!(installation.activate(&config_path).expect("activate R2"), 2);
    let b = installation.active().expect("active R2");
    assert_ne!(a.nonce, b.nonce);
    assert!(r2.is_dir());
    assert!(r1.is_dir(), "old payload root remains in place");

    write_config(root.path(), &r1, "workstation");
    assert_eq!(
        installation.activate(&config_path).expect("reactivate R1"),
        3
    );
    let c = installation.active().expect("active R1 again");
    assert_ne!(a.nonce, c.nonce);
    assert_ne!(b.nonce, c.nonce);
    assert_eq!(c.epoch, 3);

    assert_eq!(installation.retire().expect("retire"), 4);
    assert_eq!(
        installation.status().expect("retired status"),
        (Lifecycle::Retired, 4)
    );
    assert!(installation.active().is_err());
    assert!(installation.activate(&config_path).is_err());
    let reopened = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("reopen");
    assert!(reopened.active().is_err());
}

#[test]
fn mismatch_invalidates_epoch_even_when_config_is_restored() {
    let root = fixture();
    fs::create_dir(root.path().join("workspace")).expect("workspace");
    let anchor = root.path().join("control");
    let state = root.path().join("state");
    let config_path = write_config(root.path(), &state, "workstation");
    let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
    let installation = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
    installation.activate(&config_path).expect("activate");
    write_config(root.path(), &state, "container");
    assert_eq!(
        installation.active().err().expect("mismatch").code(),
        "installation_mismatch"
    );
    assert_eq!(
        installation.status().expect("invalidated"),
        (Lifecycle::Changing, 2)
    );
    write_config(root.path(), &state, "workstation");
    assert!(
        installation.active().is_err(),
        "restoration cannot undo invalidation"
    );
    assert_eq!(
        installation.activate(&config_path).expect("new activation"),
        3
    );
}

#[test]
fn failed_activation_retires_prior_epoch_before_config_validation() {
    let root = fixture();
    fs::create_dir(root.path().join("workspace")).expect("workspace");
    let anchor = root.path().join("control");
    let state = root.path().join("state");
    let config_path = write_config(root.path(), &state, "workstation");
    let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
    let installation = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
    installation.activate(&config_path).expect("activate");
    let bad = root.path().join("missing.toml");
    assert!(installation.activate(&bad).is_err());
    assert_eq!(
        installation.status().expect("changing"),
        (Lifecycle::Changing, 2)
    );
    assert!(installation.active().is_err());
    assert_eq!(installation.activate(&config_path).expect("recover"), 3);
}

#[test]
fn replacing_a_root_at_the_same_path_invalidates_its_inode() {
    let root = fixture();
    fs::create_dir(root.path().join("workspace")).expect("workspace");
    let anchor = root.path().join("control");
    let state = root.path().join("state");
    let config_path = write_config(root.path(), &state, "workstation");
    let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
    let installation = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
    installation.activate(&config_path).expect("activate");
    fs::rename(&state, root.path().join("old-state")).expect("retain old payload root");
    fs::create_dir(&state).expect("replacement root");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("private mode");
    assert_eq!(
        installation
            .active()
            .err()
            .expect("root replacement")
            .code(),
        "installation_mismatch"
    );
    assert_eq!(
        installation.status().expect("invalidated"),
        (Lifecycle::Changing, 2)
    );
}

#[test]
fn copied_or_corrupt_anchor_cannot_be_opened_as_the_installation() {
    let root = fixture();
    let anchor = root.path().join("control");
    let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
    let copied = root.path().join("copied-control");
    fs::create_dir(&copied).expect("copy directory");
    fs::set_permissions(&copied, fs::Permissions::from_mode(0o700)).expect("private directory");
    for entry in fs::read_dir(&anchor).expect("anchor entries") {
        let entry = entry.expect("entry");
        let destination = copied.join(entry.file_name());
        fs::copy(entry.path(), &destination).expect("copy control record");
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))
            .expect("private record");
    }
    assert_eq!(
        Installation::open(&copied, id, ClientAdapter::CodexV1)
            .err()
            .expect("copied anchor")
            .code(),
        "installation_invalid"
    );
    fs::write(anchor.join("installation-manifest"), b"broken").expect("corrupt manifest");
    assert_eq!(
        Installation::open(&anchor, id, ClientAdapter::CodexV1)
            .err()
            .expect("corrupt anchor")
            .code(),
        "installation_invalid"
    );
}

#[test]
fn activation_and_repair_reject_epoch_overflow_without_wrapping_authority() {
    for unresolved in [false, true] {
        let root = fixture();
        fs::create_dir(root.path().join("workspace")).expect("workspace");
        let anchor = root.path().join("control");
        let config = write_config(root.path(), &root.path().join("state"), "workstation");
        let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
        let installation = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
        installation.activate(&config).expect("activate");
        let manifest_path = anchor.join("installation-manifest");
        let journal_path = anchor.join("activation-journal");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest bytes"))
                .expect("manifest JSON");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&journal_path).expect("journal bytes"))
                .expect("journal JSON");
        manifest["epoch"] = serde_json::json!(u64::MAX);
        journal["epoch"] = serde_json::json!(u64::MAX);
        journal["resolved"] = serde_json::json!(!unresolved);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("manifest encoding"),
        )
        .expect("manifest replacement");
        fs::write(
            &journal_path,
            serde_json::to_vec(&journal).expect("journal encoding"),
        )
        .expect("journal replacement");
        let reopened = Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("reopen");
        let result = if unresolved {
            reopened.repair_incomplete()
        } else {
            reopened.activate(&config)
        };
        assert_eq!(
            result.expect_err("epoch must not wrap").code(),
            "installation_invalid"
        );
        let again =
            Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("reopen after refusal");
        assert_eq!(again.status().is_ok(), !unresolved);
        assert!(again.repair_incomplete().is_err());
    }
}
