//! Claimed-generation, audit and filesystem failure recovery acceptance.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, mkfifoat};
use serde_json::{Value, json};

use super::{Rig, isolated, run};
use crate::config::ClientAdapter;
use crate::pre_tool::rust_gate::tests::FixtureRuntime;

#[test]
fn superseding_delivery_survives_old_rust_claim_but_changed_pack_retires_parent() {
    if isolated(
        "supervised::recovery::superseding_delivery_survives_old_rust_claim_but_changed_pack_retires_parent",
    ) {
        return;
    }
    for delegated in [false, true] {
        let rig = Rig::new(ClientAdapter::ClaudeV1);
        let (reason, neutral) = run(
            &rig,
            &json!({"command":"cargo build"}),
            delegated,
            "supersede",
        );
        assert!(!neutral);
        assert_eq!(reason.code(), "snapshot_invalid");
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
        let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), delegated, "policy");
        assert!(!neutral);
        assert_eq!(reason.code(), "policy_changed");
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
    }
}

#[test]
fn v3_audit_capacity_and_corruption_never_complete_a_claim() {
    if isolated("supervised::recovery::v3_audit_capacity_and_corruption_never_complete_a_claim") {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        for corruption in [false, true] {
            let rig = Rig::new(adapter);
            if !corruption {
                let path = rig.root.path().join("hooks.toml");
                let mut document: toml::Value =
                    toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                document["gate"]["audit_max_records"] = 1_i64.into();
                fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
                rig.installation.activate(&path).unwrap();
            }
            assert!(run(&rig, &json!({"command":"cargo build"}), false, "disk").1);
            assert_eq!(rig.last_audit()["version"], 3);
            if corruption {
                fs::write(
                    rig.root.path().join("control/audit-00000001"),
                    "SECRET_CORRUPTION",
                )
                .unwrap();
            }
            let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), false, "disk");
            assert!(!neutral);
            assert_eq!(reason.code(), "audit_failed");
            assert!(rig.installation.read_snapshot("session", &rig.cwd).is_err());
            let counter: Value = serde_json::from_slice(
                &fs::read(rig.root.path().join("control/audit-counter")).unwrap(),
            )
            .unwrap();
            assert_eq!(counter["records"], 1);
            assert!(!rig.tools.path().join("marker").exists());
        }
    }
}

#[test]
fn nonregular_inaccessible_scripts_and_late_storage_are_safe_denials() {
    if isolated(
        "supervised::recovery::nonregular_inaccessible_scripts_and_late_storage_are_safe_denials",
    ) {
        return;
    }
    for kind in [
        "fifo",
        "inaccessible",
        "source-alias",
        "lock-alias",
        "build-script",
    ] {
        let rig = Rig::with_contract(ClientAdapter::ClaudeV1, false, false);
        fs::create_dir(rig.cwd.join(".cargo")).unwrap();
        let config = rig.cwd.join(".cargo/config");
        match kind {
            "fifo" => mkfifoat(rustix::fs::CWD, &config, Mode::from_bits_truncate(0o600)).unwrap(),
            "inaccessible" => {
                fs::write(&config, "[build]\ntarget-dir='/disk/target'\n").unwrap();
                fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
            }
            "source-alias" => symlink("/tmp", rig.cwd.join("src/escape")).unwrap(),
            "lock-alias" => {
                let target = rig.root.path().join("external-lock");
                fs::write(&target, "LOCK_SENTINEL").unwrap();
                symlink(&target, rig.cwd.join("Cargo.lock")).unwrap();
                fs::write(rig.cwd.join("direct.rs"), "fn main() {}\n").unwrap();
            }
            _ => {
                fs::write(
                    rig.cwd.join("Cargo.toml"),
                    "[package]\nname='literal'\nversion='1.0.0'\n",
                )
                .unwrap();
                fs::write(rig.cwd.join("build.rs"), "fn main(){std::fs::write(\"marker\",\"launched\").unwrap();panic!(\"SENTINEL_SCRIPT\");}\n").unwrap();
            }
        }
        let start = Instant::now();
        // chmod alone is insufficient for root/CAP_DAC_OVERRIDE CI workers.
        // The private reader supplies a deterministic inaccessible observation.
        let mode = if kind == "inaccessible" {
            "unreadable"
        } else {
            "disk"
        };
        let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), true, mode);
        assert!(!neutral, "{kind}");
        assert_eq!(reason.code(), "rust_analysis_indeterminate");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!rig.cwd.join("marker").exists());
        assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
        assert_eq!(rig.last_audit()["rust"]["candidates"], 0);
        if kind == "lock-alias" {
            let (reason, neutral) = run(
                &rig,
                &json!({"command":"rustdoc direct.rs -o /disk/docs"}),
                true,
                "disk",
            );
            assert!(!neutral);
            assert_eq!(reason.code(), "rust_analysis_indeterminate");
            assert_eq!(
                fs::read_to_string(rig.root.path().join("external-lock")).unwrap(),
                "LOCK_SENTINEL"
            );
            assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
        }
        if kind == "inaccessible" {
            fs::set_permissions(config, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let rig = Rig::with_contract(ClientAdapter::ClaudeV1, false, false);
    let runtime = FixtureRuntime {
        late: true,
        ..FixtureRuntime::default()
    };
    let failure = rig.run("cargo build", true, &runtime).err().unwrap();
    assert_eq!(runtime.calls.get(), 1, "the report actually completed late");
    assert_eq!(failure.reason.code(), "rust_analysis_indeterminate");
    let record = rig.last_audit();
    assert_eq!(record["rust"]["analysis"], "indeterminate");
    assert_eq!(record["rust"]["storage"], Value::Null);
    assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
}
