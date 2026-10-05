//! Public scope, structured conversion-manifest and real read-only probes.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use memory_common::guardrails::GuardrailPack;
use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors, PolicyWrite};
use memory_hooks::storage::{
    Candidate, CandidateRole, ExecutionDestination, Outcome, Reason, evaluate,
};
use memory_hooks::{ResolvedBinding, TrustedHooksConfig};
use uuid::Uuid;

const LOCAL: ExecutionDestination = ExecutionDestination::SameNamespaceAndRoot;

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let directory = tempfile::tempdir_in(base).unwrap();
        let root = directory.path().join("work");
        fs::create_dir(&root).unwrap();
        let config = directory.path().join("hooks.toml");
        Self {
            directory,
            root,
            config,
        }
    }
    fn source(&self, version: u8, profile: &str) -> String {
        format!(
            "schema_version = {version}\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"work\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = {profile:?}\nlanguage = [\"rust\"]\n",
            self.directory.path().join("state").to_str().unwrap(),
            self.root.to_str().unwrap()
        )
    }
    fn load(&self, source: &str) -> TrustedHooksConfig {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&self.config)
            .unwrap();
        file.write_all(source.as_bytes()).unwrap();
        TrustedHooksConfig::load(&self.config).unwrap()
    }
    fn binding(&self, profile: &str) -> ResolvedBinding {
        self.load(&self.source(1, profile))
            .resolve(&self.root)
            .unwrap()
    }
}

fn manifest_rule(name: &str) -> CanonicalRule {
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../docs/manifests/rust-storage-2026-09-26.json"
    ))
    .unwrap();
    let item = &manifest[name];
    let metadata: PolicyWrite = serde_json::from_value(item["policy"].clone()).unwrap();
    CanonicalRule {
        project: "general".to_owned(),
        id: serde_json::from_value(item["id"].clone()).unwrap(),
        policy_key: Some(metadata.policy_key),
        revision: Some(metadata.revision),
        delivery_class: Some(DeliveryClass::Mandatory),
        selectors: metadata.selectors,
        values: metadata.values,
        content: item["content"].as_str().unwrap().to_owned(),
        overrides: None,
    }
}
fn pack(binding: &ResolvedBinding, rules: Vec<CanonicalRule>) -> GuardrailPack {
    GuardrailPack::new(
        binding.project().to_owned(),
        binding.context().clone(),
        1,
        rules,
    )
    .unwrap()
}
fn target(path: &Path) -> Candidate {
    Candidate::new(0, CandidateRole::TargetDirectory, path.as_os_str()).unwrap()
}

#[test]
fn registry_query_retains_unknown_mandates_without_path_probes() {
    use memory_hooks::storage::{requires_rust_analysis, scope_digest};
    let fixture = Fixture::new();
    let binding = fixture.binding("workstation-host");
    let mut rule = manifest_rule("host_active");
    assert!(requires_rust_analysis(&binding, &pack(&binding, vec![rule.clone()])).unwrap());
    rule.values = [(
        "rust.build.future_storage".to_owned(),
        "unknown-secret".to_owned(),
    )]
    .into();
    assert!(requires_rust_analysis(&binding, &pack(&binding, vec![rule.clone()])).unwrap());
    rule.values.clear();
    rule.content = "rust.build.target_storage=persistent-disk /secret".to_owned();
    let empty = pack(&binding, vec![rule]);
    assert!(!requires_rust_analysis(&binding, &empty).unwrap());
    assert_eq!(
        scope_digest(&binding).unwrap(),
        evaluate(&binding, &empty, LOCAL, &[]).scope_hash()
    );
    let mut corrupt = empty;
    corrupt.digest = "unknown-secret".to_owned();
    assert!(requires_rust_analysis(&binding, &corrupt).is_err());
}

#[test]
fn storage_scope_cannot_reset_an_expired_operation_deadline() {
    let fixture = Fixture::new();
    let binding = fixture.binding("workstation-host");
    let pack = pack(&binding, vec![manifest_rule("host_active")]);
    let expired = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(matches!(
        memory_hooks::storage::EvaluationScope::observe_before(&binding, &pack, LOCAL, expired),
        Err(Reason::Limit)
    ));
}

#[test]
fn structured_registry_scope_selectors_unknown_and_prose() {
    let fixture = Fixture::new();
    let binding = fixture.binding("workstation-host");
    let original = manifest_rule("host_active");
    let baseline = pack(&binding, vec![original.clone()]);
    let report = evaluate(&binding, &baseline, LOCAL, &[target(&fixture.root)]);
    let mut prose = original.clone();
    prose.content = "SENTINEL_SECRET arbitrary path /never/reveal".to_owned();
    let changed = evaluate(
        &binding,
        &pack(&binding, vec![prose]),
        LOCAL,
        &[target(&fixture.root)],
    );
    assert_eq!(report.outcome(), changed.outcome());
    for (before, after) in report.assessments().iter().zip(changed.assessments()) {
        assert_eq!(before.outcome(), after.outcome());
        assert_eq!(before.reason(), after.reason());
        assert_eq!(before.policy(), after.policy());
        assert_eq!(before.evidence(), after.evidence());
        assert_eq!(before.pack_digest(), report.pack_digest());
        assert_eq!(after.pack_digest(), changed.pack_digest());
    }
    assert_ne!(report.pack_digest(), changed.pack_digest());
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("SENTINEL")
    );
    for (key, value) in [
        ("rust.build.target_storage", "future-secret"),
        ("rust.build.compiler_tmp_storage", "container-local-tmp"),
        ("rust.build.future_storage", "persistent-disk"),
        ("rust.build.storage.future", "secret"),
    ] {
        let mut rule = original.clone();
        rule.values = [(key.to_owned(), value.to_owned())].into();
        let result = evaluate(
            &binding,
            &pack(&binding, vec![rule]),
            LOCAL,
            &[target(&fixture.root)],
        );
        assert_eq!(result.outcome(), Outcome::Indeterminate);
        assert_eq!(
            result.assessments()[0].reason(),
            Reason::UnsupportedConstraint
        );
        assert!(result.assessments()[0].policy().is_some());
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("future-secret")
        );
    }
    let mut selector = original.clone();
    selector.selectors.profile = Some(["woodpecker-container".to_owned()].into());
    let result = evaluate(
        &binding,
        &pack(&binding, vec![selector]),
        LOCAL,
        &[target(Path::new("/does/not/exist"))],
    );
    assert_eq!(result.assessments()[0].reason(), Reason::SelectorMismatch);
    assert!(result.assessments()[0].policy().is_some());
    assert!(result.assessments()[0].evidence().is_none());
    let mut unknown = original.clone();
    unknown.selectors.tool = Some(["cargo".to_owned()].into());
    assert_eq!(
        evaluate(&binding, &pack(&binding, vec![unknown]), LOCAL, &[]).outcome(),
        Outcome::Indeterminate
    );
    let mut wrong = baseline.clone();
    wrong.project = "different".to_owned();
    assert_eq!(
        evaluate(&binding, &wrong, LOCAL, &[]).assessments()[0].reason(),
        Reason::InvalidPack
    );
    let mut wrong = baseline.clone();
    wrong.context.profile = Some("woodpecker-container".to_owned());
    assert_eq!(
        evaluate(&binding, &wrong, LOCAL, &[]).assessments()[0].reason(),
        Reason::InvalidPack
    );
    let mut wrong = baseline.clone();
    wrong.digest = "SENTINEL_SECRET".to_owned();
    let result = evaluate(&binding, &wrong, LOCAL, &[]);
    assert_eq!(result.assessments()[0].reason(), Reason::InvalidPack);
    assert!(!serde_json::to_string(&result).unwrap().contains("SENTINEL"));
    let mut oversized = baseline.clone();
    oversized.mandatory[0].content = "x".repeat(16385);
    assert_eq!(
        evaluate(&binding, &oversized, LOCAL, &[]).assessments()[0].reason(),
        Reason::Limit
    );
    assert_eq!(
        evaluate(&binding, &baseline, ExecutionDestination::Unknown, &[]).assessments()[0].reason(),
        Reason::UnknownDestination
    );
}

#[test]
fn conflicts_all_policies_role_coverage_and_valid_pack_extremes() {
    let fixture = Fixture::new();
    let binding = fixture.binding("workstation-host");
    let original = manifest_rule("host_active");
    let mut container = original.clone();
    container.id = Uuid::from_u128(20);
    container.project = "memory-server".to_owned();
    container.values = [(
        "rust.build.target_storage".to_owned(),
        "container-local-tmp".to_owned(),
    )]
    .into();
    let result = evaluate(
        &binding,
        &pack(&binding, vec![original.clone(), container]),
        LOCAL,
        &[target(&fixture.root)],
    );
    assert_eq!(result.outcome(), Outcome::Indeterminate);
    assert_eq!(result.assessments().len(), 2);
    assert!(
        result
            .assessments()
            .iter()
            .all(|item| item.reason() == Reason::ConflictingRequirements)
    );
    let mut unconstrained = original.clone();
    unconstrained.values = BTreeMap::new();
    unconstrained.selectors = PolicySelectors::default();
    assert_eq!(
        evaluate(
            &binding,
            &pack(&binding, vec![unconstrained]),
            LOCAL,
            &[target(&fixture.root)]
        )
        .outcome(),
        Outcome::NotApplicable
    );
    assert_eq!(
        evaluate(
            &binding,
            &pack(&binding, vec![original.clone()]),
            LOCAL,
            &[]
        )
        .outcome(),
        Outcome::Indeterminate
    );
    let many = (1..=32)
        .map(|id| {
            let mut rule = original.clone();
            rule.id = Uuid::from_u128(id);
            rule.policy_key = Some(format!("rust.build.storage.fixture{id:02}"));
            rule.content = "Structured storage fixture".to_owned();
            rule
        })
        .collect();
    let result = evaluate(
        &binding,
        &pack(&binding, many),
        LOCAL,
        &[target(&fixture.root)],
    );
    assert_eq!(result.assessments().len(), 32);
    let container_binding = fixture.binding("woodpecker-container");
    let mut container = original;
    container.selectors.profile = Some(["woodpecker-container".to_owned()].into());
    container.values = [(
        "rust.build.target_storage".to_owned(),
        "container-local-tmp".to_owned(),
    )]
    .into();
    let p = pack(&container_binding, vec![container]);
    let source =
        Candidate::new(0, CandidateRole::SourceWorktree, fixture.root.as_os_str()).unwrap();
    assert_eq!(
        evaluate(&container_binding, &p, LOCAL, &[source]).outcome(),
        Outcome::NotApplicable
    );
    assert_eq!(
        evaluate(
            &container_binding,
            &p,
            LOCAL,
            &[target(Path::new("/tmp/target"))]
        )
        .outcome(),
        Outcome::Indeterminate
    );
}

#[test]
fn real_readonly_namespace_directory_symlinks_and_backing() {
    let fixture = Fixture::new();
    let binding = fixture.binding("workstation-host");
    let p = pack(&binding, vec![manifest_rule("host_active")]);
    let report = evaluate(&binding, &p, LOCAL, &[target(&fixture.root)]);
    let metadata = rustix::fs::statfs(&fixture.root).unwrap();
    if metadata.f_type == libc::TMPFS_MAGIC {
        assert_eq!(report.outcome(), Outcome::Deny, "{report:?}");
    } else {
        eprintln!(
            "SKIP live tmpfs probe: fixture root is outside tmpfs; deterministic memory fixtures ran"
        );
    }
    let nested = fixture.root.join("missing/deeper");
    let report = evaluate(&binding, &p, LOCAL, &[target(&nested)]);
    assert!(
        report.assessments()[0]
            .evidence()
            .as_ref()
            .unwrap()
            .missing_suffix()
    );
    let alias = fixture.root.join("alias");
    symlink("missing", &alias).unwrap();
    assert_eq!(
        evaluate(&binding, &p, LOCAL, &[target(&alias)]).assessments()[0].reason(),
        Reason::DanglingSymlink
    );
    let alias = fixture.root.join("ordinary");
    symlink(".", &alias).unwrap();
    assert_eq!(
        evaluate(&binding, &p, LOCAL, &[target(&alias)]).outcome(),
        evaluate(&binding, &p, LOCAL, &[target(&fixture.root)]).outcome()
    );
    if rustix::process::geteuid().as_raw() != 0 {
        let inaccessible = fixture.root.join("inaccessible");
        fs::create_dir(&inaccessible).unwrap();
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o0)).unwrap();
        let denied = evaluate(&binding, &p, LOCAL, &[target(&inaccessible)]);
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(denied.outcome(), Outcome::Indeterminate);
        assert_eq!(denied.assessments()[0].reason(), Reason::Inaccessible);
    } else {
        eprintln!("SKIP live permission probe: effective UID is root; injected failures ran");
    }
    assert_eq!(
        evaluate(&binding, &p, LOCAL, &[target(Path::new("/proc/self/fd/0"))]).assessments()[0]
            .reason(),
        Reason::SpecialIndirection
    );
    for disk in ["/run/host/boot", "/boot"] {
        if Path::new(disk).is_dir() {
            let result = evaluate(&binding, &p, LOCAL, &[target(Path::new(disk))]);
            if result.outcome() == Outcome::Pass {
                eprintln!("live supported disk probe: Pass");
                return;
            }
        }
    }
    eprintln!(
        "SKIP live supported disk Pass: no accessible supported disk with coherent sysfs evidence; deterministic disk fixtures ran"
    );
}

#[test]
fn v5_compatibility_provisioning_fingerprint_and_stale_proof() {
    let fixture = Fixture::new();
    let example = include_str!("../../memory-hooks-v4.toml.example");
    let (_, managed) = example.split_once("[transport]").unwrap();
    let managed = format!("[transport]{managed}")
        .replace("api_token = \"replace-with-protected-bearer-token\"\n", "")
        .replace("unauthenticated = false", "unauthenticated = true");
    let proof = format!(
        "[storage_execution]\ncontract = \"isolated-woodpecker-storage-v1\"\nbinding_root = {:?}\nproject = \"general\"\nboot_id = \"00000000-0000-0000-0000-000000000001\"\nnamespace_device = 9\nnamespace_inode = 10\nroot_device = 1\nroot_inode = 1\nisolated_woodpecker = true\n[[storage_execution.mounts]]\nid = 2\nroot = \"/\"\nmajor = 0\nminor = 2\nmountpoint = \"/tmp\"\nmountpoint_device = 2\nmountpoint_inode = 3\nfilesystem = \"tmpfs\"\nprovenance = \"container-local\"\n",
        fixture.root.to_str().unwrap()
    );
    let source = format!(
        "{}{managed}{proof}",
        fixture.source(5, "woodpecker-container")
    );
    let first = fixture.load(&source);
    assert!(first.fingerprint().starts_with("v5:"));
    assert!(first.delivery().is_some());
    assert!(first.gate().is_some());
    assert!(first.delegation().is_some());
    let binding = first.resolve(&fixture.root).unwrap();
    let mut rule = manifest_rule("host_active");
    rule.values = [(
        "rust.build.target_storage".to_owned(),
        "container-local-tmp".to_owned(),
    )]
    .into();
    rule.selectors.profile = Some(["woodpecker-container".to_owned()].into());
    let result = evaluate(
        &binding,
        &pack(&binding, vec![rule]),
        LOCAL,
        &[target(Path::new("/tmp/target"))],
    );
    assert_eq!(result.outcome(), Outcome::Deny, "{result:?}");
    assert_eq!(
        result.assessments()[0].reason(),
        Reason::AttestationMismatch
    );
    for (before, after) in [
        ("namespace_device = 9", "namespace_device = 11"),
        ("namespace_inode = 10", "namespace_inode = 11"),
        ("root_inode = 1", "root_inode = 2"),
        ("root_device = 1", "root_device = 2"),
        ("minor = 2", "minor = 4"),
        ("mountpoint_inode = 3", "mountpoint_inode = 4"),
        (
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
        ),
    ] {
        assert_ne!(
            first.fingerprint(),
            fixture.load(&source.replace(before, after)).fingerprint()
        );
    }
    for altered in [
        source.replace("schema_version = 5", "schema_version = 4"),
        source.replace("woodpecker-container", "workstation-host"),
        source.replace("isolated_woodpecker = true", "isolated_woodpecker = false"),
        source.replace("container-local\"", "host-imported\""),
        source.replace("namespace_inode = 10", "namespace_inode = 0"),
        source.replace("\nproject = \"general\"", "\nproject = \"unknown\""),
        source.replace(
            "contract = \"managed-pre-tool-v1\"",
            "contract = \"unknown\"",
        ),
    ] {
        let mut file = OpenOptions::new()
            .truncate(true)
            .write(true)
            .open(&fixture.config)
            .unwrap();
        file.write_all(altered.as_bytes()).unwrap();
        assert!(TrustedHooksConfig::load(&fixture.config).is_err());
    }
    let second_mount = "[[storage_execution.mounts]]\nid = 1\nroot = \"/\"\nmajor = 0\nminor = 1\nmountpoint = \"/\"\nmountpoint_device = 1\nmountpoint_inode = 1\nfilesystem = \"ext4\"\nprovenance = \"container-local\"\n";
    let unordered = format!("{source}{second_mount}");
    let ordered = unordered.replace(
        &format!("{proof}{second_mount}"),
        &format!(
            "{}{second_mount}{}",
            proof.split_once("[[storage_execution.mounts]]").unwrap().0,
            proof
                .split_once("[[storage_execution.mounts]]")
                .map(|(_, tail)| format!("[[storage_execution.mounts]]{tail}"))
                .unwrap()
        ),
    );
    assert_eq!(
        fixture.load(&unordered).fingerprint(),
        fixture.load(&ordered).fingerprint()
    );
}
