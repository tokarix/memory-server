//! Supervised linked-worktree and literal source path coverage.

use std::fs;
use std::process::Command;

use serde_json::json;

use super::{Rig, isolated, run};
use crate::config::ClientAdapter;

fn git(rig: &Rig, arguments: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(&rig.cwd)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn linked_callback_builds_but_cross_worktree_execution_cannot_rebind() {
    if isolated(
        "supervised::worktrees::linked_callback_builds_but_cross_worktree_execution_cannot_rebind",
    ) {
        return;
    }
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let mut rig = Rig::new(adapter);
        git(&rig, &["init", "-q"]);
        git(&rig, &["add", "Cargo.toml", "src/main.rs"]);
        git(&rig, &["commit", "-qm", "fixture"]);
        let primary = rig.cwd.clone();
        let linked = rig.root.path().join("linked source with spaces");
        git(
            &rig,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                linked.to_str().unwrap(),
            ],
        );
        let config = rig.root.path().join("hooks.toml");
        let mut document: toml::Value =
            toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        let binding = document["bindings"][0].as_table_mut().unwrap();
        binding.remove("root");
        binding.remove("allow_subdirectories");
        binding.insert("kind".to_owned(), "git".into());
        binding.insert(
            "common_dir".to_owned(),
            primary.join(".git").to_str().unwrap().into(),
        );
        binding.insert(
            "primary_worktree".to_owned(),
            primary.to_str().unwrap().into(),
        );
        fs::write(&config, toml::to_string(&document).unwrap()).unwrap();
        rig.installation.activate(&config).unwrap();
        rig.cwd = linked;
        for child in [false, true] {
            if child && adapter == ClientAdapter::CodexV1 {
                continue;
            }
            let (reason, neutral) = run(&rig, &json!({"command":"cargo build"}), child, "disk");
            assert!(neutral, "linked callback: {reason:?}");
            let (reason, neutral) = run(
                &rig,
                &json!({"argv":["cargo", "check"], "workdir":primary}),
                child,
                "disk",
            );
            assert!(!neutral);
            assert_eq!(reason.code(), "rust_execution_unsupported");
            if child {
                assert!(rig.installation.read_snapshot("session", &rig.cwd).is_ok());
            }
        }
        fs::write(rig.cwd.join("source with spaces.rs"), "fn main() {}\n").unwrap();
        let (reason, neutral) = run(
            &rig,
            &json!({"command":"rustdoc --crate-name literal -o '/disk/docs with spaces' 'source with spaces.rs'"}),
            false,
            "disk",
        );
        assert!(neutral, "literal source/output spaces: {reason:?}");
        assert!(!rig.tools.path().join("marker").exists());
    }
}
