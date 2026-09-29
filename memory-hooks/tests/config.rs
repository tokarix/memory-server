//! Focused trusted-path and v1 schema checks for the first commit.
#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use memory_hooks::{TrustedHooksConfig, config::DelegationPin};

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

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "URL rejection matrix and fingerprint cases share one fixture"
)]
fn v2_root_origin_and_managed_client_are_strict() {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    let plain = fixture.path().join("plain");
    fs::create_dir(&plain).expect("directory");
    let path = fixture.path().join("hooks.toml");
    let base = format!(
        "schema_version = 2\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n",
        fixture.path().join("state").display().to_string(),
        plain.display().to_string()
    );
    let client = "[client]\nadapter = \"codex-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\nadditional_context_limit = 0\n";
    let write = |url: &str, token: Option<&str>, revision: &str, client: &str| {
        let auth = token.map_or_else(
            || "unauthenticated = true\n".to_owned(),
            |value| format!("unauthenticated = false\napi_token = {value:?}\n"),
        );
        let source = format!(
            "{base}[transport]\nmemoryd_url = {url:?}\n{auth}credential_revision = {revision:?}\n{client}"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .expect("config file");
        file.write_all(source.as_bytes()).expect("config write");
    };

    write("https://example.invalid", None, "initial", client);
    let first = TrustedHooksConfig::load(&path).expect("root URL");
    assert_eq!(
        first.delivery().expect("v2").origin(),
        "https://example.invalid/"
    );
    assert!(first.fingerprint().starts_with("v2:"));
    write("https://example.invalid/", None, "initial", client);
    let slash = TrustedHooksConfig::load(&path).expect("equivalent slash");
    assert_eq!(first.fingerprint(), slash.fingerprint());
    write("https://example.invalid/", None, "changed", client);
    assert_ne!(
        first.fingerprint(),
        TrustedHooksConfig::load(&path)
            .expect("revision")
            .fingerprint()
    );
    write(
        "https://example.invalid/",
        Some("SENTINEL_SECRET"),
        "initial",
        client,
    );
    let bearer = TrustedHooksConfig::load(&path).expect("explicit bearer");
    assert_ne!(first.fingerprint(), bearer.fingerprint());
    assert!(!bearer.fingerprint().contains("SENTINEL_SECRET"));
    assert_eq!(
        bearer.delivery().expect("v2").api_token(),
        Some("SENTINEL_SECRET")
    );
    write(
        "https://example.invalid/",
        Some("CHANGED_SECRET"),
        "initial",
        client,
    );
    assert_eq!(
        bearer.fingerprint(),
        TrustedHooksConfig::load(&path)
            .expect("token rotation requires revision")
            .fingerprint()
    );

    for url in [
        "https://example.invalid/memoryd",
        "https://example.invalid/memoryd/",
        "https://example.invalid//",
        "https://example.invalid/%2f",
        "https://example.invalid/.",
        "https://example.invalid/..",
        "https://example.invalid/./",
        "https://example.invalid\\memoryd",
        "https://user@example.invalid/",
        "https://example.invalid/?",
        "https://example.invalid/#",
        "https://example.invalid/ ",
        "https://example.invalid/\n",
        "ftp://example.invalid/",
    ] {
        write(url, None, "initial", client);
        assert_eq!(
            TrustedHooksConfig::load(&path)
                .err()
                .expect("unsafe URL")
                .code(),
            "config_invalid",
            "URL case: {url:?}"
        );
    }
    write("http://127.0.0.1:8080/", None, "initial", client);
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .expect("root with port")
            .delivery()
            .expect("v2")
            .origin(),
        "http://127.0.0.1:8080/"
    );
    write(
        "https://example.invalid/",
        None,
        "initial",
        &client.replace(
            "additional_context_limit = 0",
            "additional_context_limit = 16",
        ),
    );
    assert_eq!(
        TrustedHooksConfig::load(&path)
            .err()
            .expect("spill disallowed")
            .code(),
        "config_invalid"
    );
}

#[test]
fn v3_requires_a_synchronous_bounded_audit_contract() {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    let plain = fixture.path().join("plain");
    fs::create_dir(&plain).expect("directory");
    let path = fixture.path().join("hooks.toml");
    let source = format!(
        "schema_version = 3\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n[transport]\nmemoryd_url = \"https://example.invalid/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = \"codex-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\nadditional_context_limit = 0\n[gate]\ncontract = \"managed-pre-tool-v1\"\nasync_hook = false\naudit_location = \"installation-anchor-v1\"\naudit_max_records = 4096\naudit_max_bytes = 8388608\naudit_record_bytes = 2048\n",
        fixture.path().join("state").display().to_string(),
        plain.display().to_string()
    );
    let write = |body: &str| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .expect("config file");
        file.write_all(body.as_bytes()).expect("config write");
    };
    write(&source);
    let first = TrustedHooksConfig::load(&path).expect("gate config");
    assert!(first.fingerprint().starts_with("v3:"));
    assert_eq!(first.gate().expect("gate").audit_max_records(), 4096);
    write(&source.replace("audit_max_records = 4096", "audit_max_records = 2048"));
    assert_ne!(
        first.fingerprint(),
        TrustedHooksConfig::load(&path)
            .expect("smaller audit store")
            .fingerprint()
    );
    for change in [
        source.replace(
            "async_hook = false\naudit_location",
            "async_hook = true\naudit_location",
        ),
        source.replace("audit_max_records = 4096", "audit_max_records = 4097"),
        source.replace("managed-pre-tool-v1", "unsupported"),
        source.replace("installation-anchor-v1", "workspace"),
    ] {
        write(&change);
        assert!(TrustedHooksConfig::load(&path).is_err());
    }
}

#[test]
fn v4_advisory_pin_and_capabilities_are_strict_and_fingerprinted() {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let fixture = tempfile::tempdir_in(base).expect("fixture");
    let plain = fixture.path().join("plain");
    fs::create_dir(&plain).expect("directory");
    let path = fixture.path().join("hooks.toml");
    let source = format!(
        "schema_version = 4\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n[transport]\nmemoryd_url = \"https://example.invalid/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = \"claude-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\n[gate]\ncontract = \"managed-pre-tool-v1\"\nasync_hook = false\naudit_location = \"installation-anchor-v1\"\naudit_max_records = 4096\naudit_max_bytes = 8388608\naudit_record_bytes = 2048\n[delegation]\ncontract = \"managed-delegation-v1\"\nclient_pin = \"claude-code-2.1.92\"\nmode = \"advisory\"\nmax_depth = 1\nsynchronous_start = true\nsynchronous_pre_tool = true\ncatch_all_pre_tool = true\ndelivery = \"subagent-start-context\"\nchild_identity = \"agent-id-on-pre-tool\"\nblocking_pre_tool = \"pre-tool-deny\"\nparent_spawn = \"Agent\"\nrepeat_start = \"new-child-required\"\ncross_cwd = \"same-cwd-only\"\n",
        fixture.path().join("state").display().to_string(),
        plain.display().to_string()
    );
    let write = |body: &str| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .expect("config file");
        file.write_all(body.as_bytes()).expect("config write");
    };
    write(&source);
    let trusted = TrustedHooksConfig::load(&path).expect("v4 advisory");
    assert!(trusted.fingerprint().starts_with("v4:"));
    let delegation = trusted.delegation().expect("delegation");
    assert_eq!(delegation.pin(), DelegationPin::Claude2192);
    assert_eq!(delegation.max_depth(), 1);
    for altered in [
        source.replace("mode = \"advisory\"", "mode = \"enforced\""),
        source.replace("max_depth = 1", "max_depth = 2"),
        source.replace("synchronous_start = true", "synchronous_start = false"),
        source.replace("catch_all_pre_tool = true", "catch_all_pre_tool = false"),
        source.replace("parent_spawn = \"Agent\"", "parent_spawn = \"Task\""),
        source.replace(
            "client_pin = \"claude-code-2.1.92\"",
            "client_pin = \"codex-cli-0.158.0\"",
        ),
        source.replace("schema_version = 4", "schema_version = 3"),
    ] {
        write(&altered);
        assert!(TrustedHooksConfig::load(&path).is_err());
    }
    write(&source.replace(
        "credential_revision = \"initial\"",
        "credential_revision = \"next\"",
    ));
    assert_ne!(
        trusted.fingerprint(),
        TrustedHooksConfig::load(&path)
            .expect("revision")
            .fingerprint()
    );
}
