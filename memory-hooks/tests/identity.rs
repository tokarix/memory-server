//! Real Git membership contract for the identity commit.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

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
    let original_cwd = std::env::current_dir().expect("process cwd");
    let deadline = Instant::now() + Duration::from_secs(30);
    assert!(
        config
            .resolve_execution(&primary, Some(&linked), deadline)
            .is_err()
    );
    let nested = repo.join("nested");
    fs::create_dir(&nested).expect("nested");
    let execution = config
        .resolve_execution(&primary, Some(Path::new("nested")), deadline)
        .expect("same-worktree cwd");
    assert_eq!(execution.callback().effective_cwd(), repo);
    assert_eq!(execution.binding().effective_cwd(), nested);
    assert_eq!(execution.binding().context(), primary.context());
    execution.recheck(&config).expect("unchanged handoff");
    let parent = config
        .resolve_execution(&primary, Some(Path::new("nested/..")), deadline)
        .expect("agreeing parent components");
    assert_eq!(parent.binding().effective_cwd(), repo);
    assert_eq!(std::env::current_dir().expect("process cwd"), original_cwd);
    git(&nested, &["init", "-q"]);
    assert!(execution.recheck(&config).is_err());
    assert!(matches!(
        config.resolve(&nested),
        Err(Error::IdentityUnmapped)
    ));
}

fn directory_fixture() -> (tempfile::TempDir, TrustedHooksConfig) {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    for name in ["one", "two"] {
        fs::create_dir(fixture.path().join(name)).expect("binding directory");
    }
    fs::create_dir(fixture.path().join("one/space here")).expect("execution directory");
    let source = format!(
        "schema_version=1\nstate_root={:?}\ngit_executable='/usr/bin/git'\n[[bindings]]\nkind='directory'\nlabel='one'\nroot={:?}\nallow_subdirectories=true\nguardrails_project='one'\n[bindings.context]\nprofile='host'\n[[bindings]]\nkind='directory'\nlabel='two'\nroot={:?}\nallow_subdirectories=true\nguardrails_project='two'\n[bindings.context]\nprofile='container'\n",
        fixture.path().join("state").display().to_string(),
        fixture.path().join("one").display().to_string(),
        fixture.path().join("two").display().to_string(),
    );
    let path = fixture.path().join("hooks.toml");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("config");
    file.write_all(source.as_bytes()).expect("write config");
    let config = TrustedHooksConfig::load(&path).expect("config");
    (fixture, config)
}

#[test]
fn execution_cwd_preserves_callback_scope_and_rejects_cross_binding() {
    let (fixture, config) = directory_fixture();
    let root = fixture.path().join("one");
    let callback = config.resolve(&root).expect("callback");
    let deadline = Instant::now() + Duration::from_secs(5);
    for literal in [None, Some(Path::new(".")), Some(Path::new("space here/.."))] {
        let execution = config
            .resolve_execution(&callback, literal, deadline)
            .expect("same cwd");
        assert_eq!(execution.binding().effective_cwd(), root);
        execution.recheck(&config).expect("recheck");
    }
    let nested = root.join("space here");
    for literal in [Path::new("space here"), nested.as_path()] {
        let execution = config
            .resolve_execution(&callback, Some(literal), deadline)
            .expect("nested cwd");
        assert_eq!(execution.binding().effective_cwd(), nested);
        assert_eq!(execution.callback().effective_cwd(), root);
        assert_eq!(execution.binding().project(), "one");
        execution.recheck(&config).expect("recheck");
    }
    for literal in [
        Path::new("../two"),
        Path::new("missing"),
        Path::new(""),
        Path::new("nul\0path"),
    ] {
        assert!(
            config
                .resolve_execution(&callback, Some(literal), deadline)
                .is_err()
        );
    }
    assert!(
        config
            .resolve_execution(&callback, Some(&fixture.path().join("two")), deadline)
            .is_err()
    );
    assert!(
        config
            .resolve_execution(&callback, Some(Path::new(&"x".repeat(4097))), deadline)
            .is_err()
    );
}

#[test]
fn ambiguous_symlink_cwd_and_replacement_cannot_survive_handoff() {
    use std::os::unix::fs::symlink;

    let (fixture, config) = directory_fixture();
    let root = fixture.path().join("one");
    let callback = config.resolve(&root).expect("callback");
    let deadline = Instant::now() + Duration::from_secs(5);
    symlink(root.join("space here"), root.join("alias")).expect("alias");
    for literal in [
        Path::new("alias"),
        Path::new("alias/.."),
        Path::new("../one/alias"),
    ] {
        assert!(
            config
                .resolve_execution(&callback, Some(literal), deadline)
                .is_err()
        );
    }
    let nested = root.join("space here");
    let execution = config
        .resolve_execution(&callback, Some(Path::new("space here")), deadline)
        .expect("handoff");
    fs::rename(&nested, root.join("previous")).expect("retain previous inode");
    fs::create_dir(&nested).expect("replacement");
    assert!(matches!(
        execution.recheck(&config),
        Err(Error::IdentityChanged)
    ));
    let execution = config
        .resolve_execution(&callback, None, deadline)
        .expect("root handoff");
    fs::rename(&root, fixture.path().join("previous-root")).expect("retain original root");
    fs::create_dir(&root).expect("root replacement");
    assert!(matches!(
        execution.recheck(&config),
        Err(Error::IdentityChanged)
    ));
}

#[test]
fn execution_handoff_cannot_extend_deadline_or_adopt_new_config() {
    let (fixture, config) = directory_fixture();
    let root = fixture.path().join("one");
    let callback = config.resolve(&root).expect("callback");
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("expired");
    assert!(config.resolve_execution(&callback, None, expired).is_err());
    let execution = config
        .resolve_execution(&callback, None, Instant::now() + Duration::from_millis(100))
        .expect("handoff");
    std::thread::sleep(Duration::from_millis(110));
    assert!(execution.recheck(&config).is_err());
    let config_path = fixture.path().join("hooks.toml");
    let source = fs::read_to_string(&config_path).expect("original config");
    fs::write(
        &config_path,
        source.replace("profile='host'", "profile='new-host'"),
    )
    .expect("change config");
    let changed = TrustedHooksConfig::load(&config_path).expect("changed config");
    assert!(matches!(
        changed.resolve_execution(&callback, None, Instant::now() + Duration::from_secs(5)),
        Err(Error::IdentityChanged)
    ));
}
