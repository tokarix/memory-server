//! Real Git membership contract for the identity commit.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use memory_hooks::{Error, TrustedHooksConfig};

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .current_dir(cwd)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args(args)
        .output()
        .expect("fixture Git");
    assert!(output.status.success(), "fixture Git failed: {args:?}");
}

#[test]
fn linked_and_detached_worktrees_share_only_their_configured_identity() {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    let repo = fixture.path().join("repo one");
    fs::create_dir(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "fixture"]);
    let linked = fixture.path().join("linked ünicode");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().expect("fixture"),
        ],
    );
    let source = format!(
        "schema_version = 1\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"git\"\nlabel = \"primary\"\ncommon_dir = {:?}\nprimary_worktree = {:?}\nguardrails_project = \"project-a\"\n[bindings.context]\nprofile = \"workstation\"\n",
        fixture.path().join("state").display().to_string(),
        repo.join(".git").display().to_string(),
        repo.display().to_string()
    );
    let path = fixture.path().join("hooks.toml");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("config");
    file.write_all(source.as_bytes()).expect("config write");
    let config = TrustedHooksConfig::load(&path).expect("trusted config");
    let primary = config.resolve(&repo).expect("primary");
    let other = config.resolve(&linked).expect("detached linked checkout");
    assert_eq!(primary.identity(), other.identity());
    assert_eq!(primary.project(), "project-a");
    assert_ne!(primary.worktree_root(), other.worktree_root());
    let nested = repo.join("nested");
    fs::create_dir(&nested).expect("nested");
    git(&nested, &["init", "-q"]);
    assert!(matches!(
        config.resolve(&nested),
        Err(Error::IdentityUnmapped)
    ));
}
