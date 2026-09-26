//! Isolated trust, identity, and state contracts.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use memory_hooks::{Error, PrivateStateStore, StateKey, TrustedHooksConfig};
use tempfile::TempDir;

fn git(cwd: &Path, args: &[&str]) {
    let result = Command::new("/usr/bin/git")
        .current_dir(cwd)
        .env_clear()
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args(args)
        .output()
        .expect("execute fixture git");
    assert!(result.status.success(), "fixture git failed: {args:?}");
}

fn fixture() -> TempDir {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from);
    tempfile::Builder::new()
        .prefix("memory-hooks-85-")
        .tempdir_in(base)
        .expect("tempdir")
}

fn config_text(root: &Path, binding: &str) -> String {
    format!(
        "schema_version = 1\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n\n{binding}\n",
        root.join("state").to_string_lossy()
    )
}

fn write_config(root: &Path, source: &str) -> PathBuf {
    let path = root.join("hooks.toml");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .expect("config file");
    file.write_all(source.as_bytes()).expect("config write");
    path
}

fn git_binding(repo: &Path) -> String {
    format!(
        "[[bindings]]\nkind = \"git\"\nlabel = \"primary\"\ncommon_dir = {:?}\nprimary_worktree = {:?}\nguardrails_project = \"project-a\"\n[bindings.context]\nprofile = \"workstation\"\nlanguage = []\n",
        repo.join(".git").to_string_lossy(),
        repo.to_string_lossy()
    )
}

fn directory_binding(directory: &Path, descendants: bool) -> String {
    format!(
        "[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nallow_subdirectories = {descendants}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n",
        directory.to_string_lossy()
    )
}

#[test]
fn strict_config_and_normalized_fingerprint() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let text = config_text(root.path(), &directory_binding(&directory, true));
    let path = write_config(root.path(), &text);
    let first = TrustedHooksConfig::load(&path).expect("valid config");
    let spaced = text.replace("schema_version = 1", "schema_version=1\n\n");
    write_config(root.path(), &spaced);
    let second = TrustedHooksConfig::load(&path).expect("equivalent config");
    assert_eq!(first.fingerprint(), second.fingerprint());
    let modified = spaced.replace("profile = \"workstation\"", "profile = \"container\"");
    write_config(root.path(), &modified);
    let third = TrustedHooksConfig::load(&path).expect("changed config");
    assert_ne!(first.fingerprint(), third.fingerprint());
    let invalid = modified.replace(
        "profile = \"container\"",
        "profile = \"container\"\napi_token = \"SENTINEL_SECRET\"",
    );
    write_config(root.path(), &invalid);
    let error = TrustedHooksConfig::load(&path)
        .err()
        .expect("unknown key rejected");
    assert_eq!(error.code(), "config_invalid");
    assert!(!format!("{error:?} {error}").contains("SENTINEL_SECRET"));
}

#[test]
fn config_limits_context_presence_and_scope_conflicts() {
    let root = fixture();
    let directory = root.path().join("plain");
    let child = directory.join("child");
    fs::create_dir_all(&child).expect("directories");
    let base = config_text(root.path(), &directory_binding(&directory, true));
    let path = write_config(root.path(), &base);
    let omitted = TrustedHooksConfig::load(&path).expect("unknown language");
    let known_empty = base.replace(
        "profile = \"workstation\"",
        "profile = \"workstation\"\nlanguage = []",
    );
    write_config(root.path(), &known_empty);
    let known = TrustedHooksConfig::load(&path).expect("known empty language");
    assert_ne!(omitted.fingerprint(), known.fingerprint());
    assert!(
        known
            .resolve(&directory)
            .expect("binding")
            .context()
            .language
            .as_ref()
            .is_some_and(std::collections::BTreeSet::is_empty)
    );
    let missing_profile = base.replace("profile = \"workstation\"", "");
    write_config(root.path(), &missing_profile);
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("profile required")
            .code(),
        "config_invalid"
    );
    let duplicate = format!(
        "{base}\n{}",
        directory_binding(&directory, false).replace("label = \"plain\"", "label = \"second\"")
    );
    write_config(root.path(), &duplicate);
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("duplicate identity")
            .code(),
        "config_invalid"
    );
    let overlap = format!(
        "{base}\n{}",
        directory_binding(&child, false).replace("label = \"plain\"", "label = \"child\"")
    );
    write_config(root.path(), &overlap);
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("overlap")
            .code(),
        "config_invalid"
    );
    let at_limit = base.replace(
        "guardrails_project = \"general\"",
        &format!("guardrails_project = \"{}\"", "a".repeat(256)),
    );
    write_config(root.path(), &at_limit);
    assert!(TrustedHooksConfig::load(&path).is_ok());
    write_config(
        root.path(),
        &at_limit.replace(&"a".repeat(256), &"a".repeat(257)),
    );
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("project over limit")
            .code(),
        "config_invalid"
    );
    let pad = "x".repeat(256 * 1024 - base.len() - 2);
    write_config(root.path(), &format!("{base}#{pad}\n"));
    assert!(TrustedHooksConfig::load(&path).is_ok());
    write_config(root.path(), &format!("{base}#{pad}x\n"));
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("config over limit")
            .code(),
        "config_too_large"
    );
}

#[test]
fn real_git_worktrees_share_the_binding() {
    let root = fixture();
    let repo = root.path().join("repo one");
    fs::create_dir(&repo).expect("repo dir");
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "fixture"]);
    let linked = root.path().join("linked ünicode");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().expect("UTF-8 fixture"),
        ],
    );
    let path = write_config(root.path(), &config_text(root.path(), &git_binding(&repo)));
    let trusted = TrustedHooksConfig::load(&path).expect("git config");
    let first = trusted.resolve(&repo).expect("primary binding");
    let second = trusted.resolve(&linked).expect("linked binding");
    assert_eq!(first.identity(), second.identity());
    assert_eq!(first.project(), "project-a");
    assert_eq!(second.project(), "project-a");
    assert_eq!(first.context(), second.context());
    assert_ne!(first.worktree_root(), second.worktree_root());
    git(&repo, &["checkout", "--detach", "-q"]);
    assert_eq!(
        trusted.resolve(&repo).expect("detached primary").identity(),
        first.identity()
    );
    let alias = root.path().join("checkout alias");
    std::os::unix::fs::symlink(&linked, &alias).expect("worktree alias");
    assert_eq!(
        trusted.resolve(&alias).expect("canonical alias").identity(),
        first.identity()
    );
    let nested = repo.join("nested");
    fs::create_dir(&nested).expect("nested dir");
    git(&nested, &["init", "-q"]);
    assert!(matches!(
        trusted.resolve(&nested),
        Err(Error::IdentityUnmapped)
    ));
    let sibling_parent = root.path().join("other");
    let same_basename = sibling_parent.join("repo one");
    fs::create_dir_all(&same_basename).expect("independent sibling");
    git(&same_basename, &["init", "-q"]);
    assert!(matches!(
        trusted.resolve(&same_basename),
        Err(Error::IdentityUnmapped)
    ));
}

#[test]
fn submodule_and_forged_gitfile_cannot_borrow_parent_binding() {
    let root = fixture();
    let parent = root.path().join("parent");
    let source = root.path().join("sub-source");
    fs::create_dir(&parent).expect("parent");
    fs::create_dir(&source).expect("source");
    git(&parent, &["init", "-q"]);
    git(&parent, &["commit", "--allow-empty", "-qm", "parent"]);
    git(&source, &["init", "-q"]);
    git(&source, &["commit", "--allow-empty", "-qm", "source"]);
    git(
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            source.to_str().expect("fixture"),
            "sub",
        ],
    );
    let config_path = write_config(
        root.path(),
        &config_text(root.path(), &git_binding(&parent)),
    );
    let config = TrustedHooksConfig::load(&config_path).expect("parent config");
    assert!(matches!(
        config.resolve(&parent.join("sub")),
        Err(Error::IdentityUnmapped)
    ));
    let forged = root.path().join("forged");
    fs::create_dir(&forged).expect("forged root");
    let gitfile = format!("gitdir: {}\n", parent.join(".git").display());
    fs::write(forged.join(".git"), gitfile).expect("forged gitfile");
    assert!(config.resolve(&forged).is_err());
}

#[test]
fn git_paths_with_control_bytes_are_rejected() {
    let root = fixture();
    let repo = root.path().join("line\nbreak");
    fs::create_dir(&repo).expect("control path");
    git(&repo, &["init", "-q"]);
    let path = write_config(root.path(), &config_text(root.path(), &git_binding(&repo)));
    let config = TrustedHooksConfig::load(&path).expect("control-path config");
    assert_eq!(
        config
            .resolve(&repo)
            .map(|_| ())
            .expect_err("control-byte Git output")
            .code(),
        "identity_invalid"
    );
}

#[test]
fn git_probe_timeout_and_output_limit_are_bounded() {
    let root = fixture();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    let base = config_text(root.path(), &git_binding(&repo));
    let path = write_config(root.path(), &base);
    let wrapper = root.path().join("git-wrapper");
    fs::write(&wrapper, b"#!/bin/sh\nexec /usr/bin/sleep 30\n").expect("timeout wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("wrapper mode");
    let source = base.replace("/usr/bin/git", wrapper.to_str().expect("fixture path"));
    write_config(root.path(), &source);
    let config = TrustedHooksConfig::load(&path).expect("wrapper config");
    let start = Instant::now();
    assert_eq!(
        config
            .resolve(&repo)
            .map(|_| ())
            .expect_err("Git deadline")
            .code(),
        "git_timeout"
    );
    assert!(start.elapsed() < Duration::from_secs(8));
    fs::write(
        &wrapper,
        b"#!/bin/sh\nexec /usr/bin/head -c 70000 /dev/zero\n",
    )
    .expect("oversize wrapper");
    let config = TrustedHooksConfig::load(&path).expect("wrapper config");
    assert_eq!(
        config
            .resolve(&repo)
            .map(|_| ())
            .expect_err("Git output limit")
            .code(),
        "identity_invalid"
    );
}

#[test]
fn repository_git_config_does_not_run_shell_code() {
    let root = fixture();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    let sentinel = root.path().join("GIT_CONFIG_SENTINEL");
    let script = root.path().join("fsmonitor");
    fs::write(
        &script,
        format!("#!/bin/sh\n/usr/bin/touch {}\n", sentinel.display()),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    git(
        &repo,
        &[
            "config",
            "core.fsmonitor",
            script.to_str().expect("fixture path"),
        ],
    );
    let path = write_config(root.path(), &config_text(root.path(), &git_binding(&repo)));
    let config = TrustedHooksConfig::load(&path).expect("config");
    config.resolve(&repo).expect("identity probe");
    assert!(
        !sentinel.exists(),
        "identity probes must not invoke fsmonitor"
    );
}

#[test]
fn directory_binding_stops_at_a_broken_git_marker() {
    let root = fixture();
    let directory = root.path().join("plain");
    let child = directory.join("child");
    fs::create_dir_all(&child).expect("plain child");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, true)),
    );
    let trusted = TrustedHooksConfig::load(&path).expect("directory config");
    assert_eq!(
        trusted.resolve(&child).expect("plain descendant").project(),
        "general"
    );
    fs::write(child.join(".git"), b"broken\n").expect("broken marker");
    assert_eq!(
        trusted
            .resolve(&child)
            .err()
            .expect("git marker blocks fallback")
            .code(),
        "identity_invalid"
    );
}

#[test]
fn state_keys_and_transactions_are_scoped() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let config = TrustedHooksConfig::load(&path).expect("config");
    let binding = config.resolve(&directory).expect("binding");
    let a = StateKey::new(&binding, "ab", "c").expect("key a");
    let b = StateKey::new(&binding, "a", "bc").expect("key b");
    assert_ne!(a.digest(), b.digest());
    assert_eq!(a.digest().len(), 64);
    assert!(StateKey::new(&binding, &"x".repeat(1024), "s").is_ok());
    assert_eq!(
        StateKey::new(&binding, &"x".repeat(1025), "s")
            .err()
            .expect("identifier limit")
            .code(),
        "state_insecure"
    );
    let hostile =
        StateKey::new(&binding, "../SENTINEL\n秘密", "slash/session").expect("bounded hostile IDs");
    assert_eq!(hostile.digest().len(), 64);
    assert!(
        hostile
            .digest()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    let store = PrivateStateStore::open(&config).expect("state root");
    let count = store
        .transact::<u64, _, _>(&a, |old| {
            assert!(old.is_none());
            Ok((Some(1), 1))
        })
        .expect("first write");
    assert_eq!(count, 1);
    let count = store
        .transact::<u64, _, _>(&a, |old| {
            let value = old.expect("record");
            Ok((Some(value + 1), value + 1))
        })
        .expect("second write");
    assert_eq!(count, 2);
    let mode = fs::metadata(root.path().join("state"))
        .expect("state metadata")
        .permissions();
    assert_eq!(mode.mode() & 0o777, 0o700);
}

#[test]
fn state_record_limit_preserves_last_complete_record() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let config = TrustedHooksConfig::load(&path).expect("config");
    let binding = config.resolve(&directory).expect("binding");
    let key = StateKey::new(&binding, "client", "record-limit").expect("key");
    let store = PrivateStateStore::open(&config).expect("store");
    store
        .transact::<String, _, _>(&key, |_| Ok((Some(String::new()), ())))
        .expect("empty record");
    let record = root
        .path()
        .join("state")
        .join(format!("record-{}", key.digest()));
    let empty_len = usize::try_from(fs::metadata(&record).expect("record metadata").len())
        .expect("record size");
    let exact = "a".repeat(128 * 1024 - empty_len);
    store
        .transact::<String, _, _>(&key, |_| Ok((Some(exact.clone()), ())))
        .expect("exact record limit");
    assert_eq!(
        fs::metadata(&record).expect("exact metadata").len(),
        128 * 1024
    );
    let too_large = format!("{exact}a");
    assert_eq!(
        store
            .transact::<String, _, _>(&key, |_| Ok((Some(too_large), ())))
            .expect_err("one over record limit")
            .code(),
        "state_corrupt"
    );
    let retained = store
        .transact::<String, _, _>(&key, |old| Ok((None, old)))
        .expect("read old complete record");
    assert_eq!(retained, Some(exact));
}

#[test]
fn state_rejects_symlinks_permissions_links_and_special_files() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let source = config_text(root.path(), &directory_binding(&directory, false));
    let path = write_config(root.path(), &source);
    let outside = root.path().join("outside");
    fs::create_dir(&outside).expect("outside fixture");
    let state_root = root.path().join("state");
    std::os::unix::fs::symlink(&outside, &state_root).expect("state symlink");
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("state symlink")
            .code(),
        "config_untrusted"
    );
    fs::remove_file(&state_root).expect("remove fixture symlink");
    fs::create_dir(&state_root).expect("state dir");
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o755)).expect("permissive state");
    let config = TrustedHooksConfig::load(&path).expect("config");
    assert_eq!(
        PrivateStateStore::open(&config)
            .err()
            .expect("state mode")
            .code(),
        "state_insecure"
    );
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).expect("private state");
    let binding = config.resolve(&directory).expect("binding");
    let key = StateKey::new(&binding, "client", "session").expect("key");
    let store = PrivateStateStore::open(&config).expect("state store");
    store
        .transact::<u64, _, _>(&key, |_| Ok((Some(1), ())))
        .expect("record");
    let record = state_root.join(format!("record-{}", key.digest()));
    let extra_link = state_root.join("extra-link");
    fs::hard_link(&record, &extra_link).expect("hard link");
    assert_eq!(
        store
            .transact::<u64, _, _>(&key, |old| Ok((None, old)))
            .expect_err("linked record")
            .code(),
        "state_insecure"
    );
    fs::remove_file(&extra_link).expect("remove fixture hard link");
    fs::remove_file(&record).expect("remove fixture record");
    std::os::unix::fs::symlink(&outside, &record).expect("record symlink");
    assert_eq!(
        store
            .transact::<u64, _, _>(&key, |old| Ok((None, old)))
            .expect_err("symlink record")
            .code(),
        "state_insecure"
    );
    fs::remove_file(&record).expect("remove fixture symlink");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&record)
            .status()
            .expect("mkfifo")
            .success()
    );
    assert_eq!(
        store
            .transact::<u64, _, _>(&key, |old| Ok((None, old)))
            .expect_err("FIFO record")
            .code(),
        "state_insecure"
    );
}

#[test]
fn config_rejects_untrusted_paths_and_boundaries() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let source = config_text(root.path(), &directory_binding(&directory, false));
    let path = write_config(root.path(), &source);
    assert_eq!(
        TrustedHooksConfig::load(Path::new("relative.toml"))
            .err()
            .expect("relative")
            .code(),
        "config_invalid"
    );
    let alias = root.path().join("alias.toml");
    std::os::unix::fs::symlink(&path, &alias).expect("config symlink");
    assert_eq!(
        TrustedHooksConfig::load(&alias)
            .err()
            .expect("symlink")
            .code(),
        "config_untrusted"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissive config");
    assert_eq!(
        TrustedHooksConfig::load(&path).err().expect("mode").code(),
        "config_untrusted"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("restore fixture mode");
    let inside = directory.join("hooks.toml");
    fs::copy(&path, &inside).expect("repo-local config fixture");
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o600)).expect("inside mode");
    assert_eq!(
        TrustedHooksConfig::load(&inside)
            .err()
            .expect("inside scope")
            .code(),
        "config_untrusted"
    );
    write_config(
        root.path(),
        &source.replace("schema_version = 1", "schema_version = 2"),
    );
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("version")
            .code(),
        "config_invalid"
    );
}

#[test]
fn posix_acl_is_rejected_even_without_extra_effective_access() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let status = Command::new("/usr/bin/setfacl")
        .args(["-m", "u:65534:---"])
        .arg(&path)
        .status()
        .expect("setfacl available");
    assert!(
        status.success(),
        "ACL fixture requires a POSIX ACL-capable filesystem"
    );
    assert_eq!(
        TrustedHooksConfig::load(&path).err().expect("ACL").code(),
        "config_untrusted"
    );
}

#[test]
fn cli_is_fixed_to_argv_config_and_redacts_paths() {
    let root = fixture();
    let directory = root.path().join("SENTINEL_PATH_SECRET");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let binary = env!("CARGO_BIN_EXE_memory-hooks");
    let output = Command::new(binary)
        .args(["resolve", "--config"])
        .arg(&path)
        .arg("--cwd")
        .arg(&directory)
        .env("MEMORY_SERVER_CONFIG", "/nonexistent/override.toml")
        .env("MEMORY_RESOLUTION_CONTEXT", "other")
        .env("GIT_DIR", "/nonexistent/git")
        .env("PATH", "/nonexistent")
        .env("HOME", "/nonexistent")
        .output()
        .expect("CLI");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI JSON");
    assert!(stdout.contains("\"guardrails_project\":\"general\""));
    assert!(stdout.contains("\"version\":1"));
    assert!(!stdout.contains("SENTINEL_PATH_SECRET"));
    let version = Command::new(binary)
        .arg("--version")
        .output()
        .expect("version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("0.1.0 "));
    let rejected = Command::new(binary)
        .args(["resolve", "--config"])
        .arg(&path)
        .arg("--cwd")
        .arg(&directory)
        .args(["--project", "injected"])
        .output()
        .expect("unknown option");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("config_invalid"));
}

#[test]
fn separate_git_dir_and_bare_backed_worktree() {
    let root = fixture();
    let separate = root.path().join("separate-admin");
    let checkout = root.path().join("checkout");
    git(
        root.path(),
        &[
            "init",
            "-q",
            "--separate-git-dir",
            separate.to_str().expect("fixture path"),
            checkout.to_str().expect("fixture path"),
        ],
    );
    git(&checkout, &["commit", "--allow-empty", "-qm", "fixture"]);
    let binding = format!(
        "[[bindings]]\nkind = \"git\"\nlabel = \"separate\"\ncommon_dir = {:?}\nprimary_worktree = {:?}\nguardrails_project = \"separate-project\"\n[bindings.context]\nprofile = \"workstation\"\n",
        separate.to_string_lossy(),
        checkout.to_string_lossy()
    );
    let path = write_config(root.path(), &config_text(root.path(), &binding));
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .expect("separate config")
            .resolve(&checkout)
            .expect("separate identity")
            .project(),
        "separate-project"
    );

    let bare = root.path().join("bare.git");
    git(
        root.path(),
        &["init", "--bare", "-q", bare.to_str().expect("fixture path")],
    );
    git(
        &checkout,
        &[
            "push",
            "-q",
            bare.to_str().expect("fixture path"),
            "HEAD:refs/heads/main",
        ],
    );
    let linked = root.path().join("bare-linked");
    git(
        root.path(),
        &[
            "--git-dir",
            bare.to_str().expect("fixture path"),
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().expect("fixture path"),
            "main",
        ],
    );
    let bare_binding = format!(
        "[[bindings]]\nkind = \"git\"\nlabel = \"bare\"\ncommon_dir = {:?}\nbare_backed = true\nguardrails_project = \"bare-project\"\n[bindings.context]\nprofile = \"workstation\"\n",
        bare.to_string_lossy()
    );
    write_config(root.path(), &config_text(root.path(), &bare_binding));
    let config = TrustedHooksConfig::load(&path).expect("bare config");
    assert_eq!(
        config.resolve(&linked).expect("bare-backed link").project(),
        "bare-project"
    );
    assert_eq!(
        config
            .resolve(&bare)
            .err()
            .expect("bare root is not checkout")
            .code(),
        "identity_invalid"
    );
}

#[test]
fn state_worker() {
    let Some(config_path) = std::env::var_os("MEMORY_HOOKS_WORKER_CONFIG") else {
        return;
    };
    let directory = PathBuf::from(std::env::var_os("MEMORY_HOOKS_WORKER_CWD").expect("worker cwd"));
    let iterations: usize = std::env::var("MEMORY_HOOKS_WORKER_ITERATIONS")
        .expect("worker iterations")
        .parse()
        .expect("numeric iterations");
    let config = TrustedHooksConfig::load(Path::new(&config_path)).expect("worker config");
    let binding = config.resolve(&directory).expect("worker binding");
    let key = StateKey::new(&binding, "client", "session").expect("worker key");
    let store = PrivateStateStore::open(&config).expect("worker store");
    if let Some(barrier) = std::env::var_os("MEMORY_HOOKS_WORKER_HOLD") {
        store
            .transact::<u64, (), _>(&key, |old| {
                fs::write(barrier, b"locked").expect("worker barrier");
                let _ = old;
                loop {
                    thread::park();
                }
            })
            .expect("hold transaction");
    }
    for _ in 0..iterations {
        store
            .transact::<u64, _, _>(&key, |old| Ok((Some(old.unwrap_or(0) + 1), ())))
            .expect("worker transaction");
    }
}

#[test]
fn multiple_processes_serialize_state_updates() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(
            Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", "state_worker"])
                .env("MEMORY_HOOKS_WORKER_CONFIG", &path)
                .env("MEMORY_HOOKS_WORKER_CWD", &directory)
                .env("MEMORY_HOOKS_WORKER_ITERATIONS", "20")
                .spawn()
                .expect("worker process"),
        );
    }
    for mut child in children {
        assert!(child.wait().expect("worker exit").success());
    }
    let config = TrustedHooksConfig::load(&path).expect("config");
    let binding = config.resolve(&directory).expect("binding");
    let key = StateKey::new(&binding, "client", "session").expect("key");
    let store = PrivateStateStore::open(&config).expect("store");
    let value = store
        .transact::<u64, _, _>(&key, |old| Ok((None, old)))
        .expect("read");
    assert_eq!(value, Some(80));
}

#[test]
fn new_state_objects_have_exact_modes_under_restrictive_umask() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let status = Command::new("/bin/sh")
        .args(["-c", "umask 0777; exec \"$1\" --exact state_worker", "sh"])
        .arg(std::env::current_exe().expect("test binary"))
        .env("MEMORY_HOOKS_WORKER_CONFIG", &path)
        .env("MEMORY_HOOKS_WORKER_CWD", &directory)
        .env("MEMORY_HOOKS_WORKER_ITERATIONS", "1")
        .status()
        .expect("restrictive worker");
    assert!(status.success());
    let config = TrustedHooksConfig::load(&path).expect("config");
    let binding = config.resolve(&directory).expect("binding");
    let key = StateKey::new(&binding, "client", "session").expect("key");
    let state = root.path().join("state");
    for (object, mode) in [
        (state.clone(), 0o700),
        (state.join(format!("lock-{}", key.digest())), 0o600),
        (state.join(format!("record-{}", key.digest())), 0o600),
    ] {
        assert_eq!(
            fs::metadata(object)
                .expect("private object")
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
}

#[test]
fn lock_timeout_and_process_death_recovery() {
    let root = fixture();
    let directory = root.path().join("plain");
    fs::create_dir(&directory).expect("plain directory");
    let path = write_config(
        root.path(),
        &config_text(root.path(), &directory_binding(&directory, false)),
    );
    let barrier = root.path().join("locked");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "state_worker"])
        .env("MEMORY_HOOKS_WORKER_CONFIG", &path)
        .env("MEMORY_HOOKS_WORKER_CWD", &directory)
        .env("MEMORY_HOOKS_WORKER_ITERATIONS", "0")
        .env("MEMORY_HOOKS_WORKER_HOLD", &barrier)
        .spawn()
        .expect("holding worker");
    let wait_until = Instant::now() + Duration::from_secs(5);
    while !barrier.exists() && Instant::now() < wait_until {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(barrier.exists(), "holding worker did not acquire the lock");
    let config = TrustedHooksConfig::load(&path).expect("config");
    let binding = config.resolve(&directory).expect("binding");
    let key = StateKey::new(&binding, "client", "session").expect("key");
    let store = PrivateStateStore::open(&config).expect("store");
    let error = store
        .transact::<u64, _, _>(&key, |old| Ok((None, old)))
        .expect_err("lock timeout");
    assert_eq!(error.code(), "lock_timeout");
    child.kill().expect("kill holding worker");
    child.wait().expect("reap holding worker");
    store
        .transact::<u64, _, _>(&key, |old| {
            assert!(old.is_none());
            Ok((Some(1), ()))
        })
        .expect("recovered transaction");
}
