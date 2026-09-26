//! Focused trusted-path and v1 schema checks for the first commit.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use memory_hooks::TrustedHooksConfig;

#[test]
fn typed_external_config_is_strict_and_normalized() {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    let plain = fixture.path().join("plain");
    fs::create_dir(&plain).expect("directory");
    let path = fixture.path().join("hooks.toml");
    let source = format!(
        "schema_version = 1\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n",
        fixture.path().join("state").display().to_string(),
        plain.display().to_string()
    );
    let write = |value: &str| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .expect("config file");
        file.write_all(value.as_bytes()).expect("config write");
    };
    write(&source);
    let first = TrustedHooksConfig::load(&path).expect("valid config");
    write(&source.replace("schema_version = 1", "schema_version=1\n"));
    let second = TrustedHooksConfig::load(&path).expect("equivalent TOML");
    assert_eq!(first.fingerprint(), second.fingerprint());
    write(&source.replace(
        "profile = \"workstation\"",
        "api_token = \"SENTINEL_SECRET\"",
    ));
    let error = TrustedHooksConfig::load(&path)
        .err()
        .expect("unknown field");
    assert_eq!(error.code(), "config_invalid");
    assert!(!format!("{error:?} {error}").contains("SENTINEL_SECRET"));
    assert_eq!(
        TrustedHooksConfig::load(Path::new("relative.toml"))
            .err()
            .expect("relative path")
            .code(),
        "config_invalid"
    );
}
