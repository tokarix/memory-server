//! Scripted complete evaluator fixtures; no subprocesses or privileged mounts.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::linux::{Kernel, Mount, Snapshot};
use super::path::Resolved;
use super::*;

const TABLE: &[u8] = b"1 99 0:1 / / rw - ext4 disk rw\n2 1 0:2 / /tmp rw future:bounded - tmpfs SECRET_MOUNT rw\n3 2 0:1 / /tmp/target/nested rw - ext4 disk rw\n";

#[derive(Clone)]
struct Node {
    identity: ObjectIdentity,
    mount: u64,
    link: Option<Vec<u8>>,
    accessible: bool,
}

struct Fake {
    nodes: BTreeMap<PathBuf, Node>,
    backing: BTreeMap<u64, BackingClass>,
    calls: usize,
    changed: bool,
    race_after_backing: bool,
    execution_race: bool,
    injected: Option<i32>,
}

impl Fake {
    fn new() -> Self {
        let mut nodes = BTreeMap::new();
        for (inode, path, mount, device) in [
            (1, "/", 1, 1),
            (2, "/disk", 1, 1),
            (3, "/tmp", 2, 2),
            (4, "/tmp/target", 2, 2),
            (5, "/tmp/target/out", 2, 2),
            (6, "/disk/source", 1, 1),
            (7, "/tmp/target/nested", 3, 1),
        ] {
            nodes.insert(
                PathBuf::from(path),
                Node {
                    identity: ObjectIdentity {
                        device,
                        inode,
                        mode: libc::S_IFDIR,
                    },
                    mount,
                    link: None,
                    accessible: true,
                },
            );
        }
        Self {
            nodes,
            backing: [
                (1, BackingClass::Disk),
                (2, BackingClass::Memory),
                (3, BackingClass::Disk),
            ]
            .into(),
            calls: 0,
            changed: false,
            race_after_backing: false,
            execution_race: false,
            injected: None,
        }
    }
    fn boundary(&mut self, budget: &mut Budget) -> ProbeResult<()> {
        budget.tick()?;
        self.calls += 1;
        Ok(())
    }
    fn symlink(&mut self, path: &str, target: &[u8]) {
        self.nodes.insert(
            PathBuf::from(path),
            Node {
                identity: ObjectIdentity {
                    device: 1,
                    inode: 100 + self.nodes.len() as u64,
                    mode: libc::S_IFLNK,
                },
                mount: 1,
                link: Some(target.to_vec()),
                accessible: true,
            },
        );
    }
    fn report(
        self,
        kind: ConstraintKind,
        candidates: &[Candidate],
        attestation: Option<attestation::StorageExecution>,
    ) -> Report {
        Engine {
            cwd: PathBuf::from("/disk"),
            profile: Some(
                if kind == ConstraintKind::ContainerTarget {
                    "woodpecker-container"
                } else {
                    "workstation-host"
                }
                .to_owned(),
            ),
            scope_hash: "scope".to_owned(),
            pack_digest: "digest".to_owned(),
            requirements: vec![requirement(kind, 1)],
            provider: self,
            initial: Snapshot::fixture(TABLE),
            attestation,
            budget: Budget::new(),
        }
        .evaluate(candidates)
    }
}

impl Provider for Fake {
    type Handle = PathBuf;
    fn root(&mut self, budget: &mut Budget) -> ProbeResult<PathBuf> {
        self.boundary(budget)?;
        Ok(PathBuf::from("/"))
    }
    fn entry(
        &mut self,
        parent: &PathBuf,
        name: &OsStr,
        budget: &mut Budget,
    ) -> ProbeResult<Entry<PathBuf>> {
        self.boundary(budget)?;
        if let Some(code) = self.injected {
            return Err(ProbeError {
                reason: Reason::Inaccessible,
                code: Some(code),
            });
        }
        let path = parent.join(name);
        let node = self.nodes.get(&path).ok_or(ProbeError {
            reason: Reason::Inaccessible,
            code: Some(libc::ENOENT),
        })?;
        let mut identity = node.identity;
        if self.changed {
            identity.inode += 1000;
        }
        Ok(if node.link.is_some() {
            Entry::Symlink(path, identity)
        } else if identity.mode & libc::S_IFMT == libc::S_IFDIR {
            Entry::Directory(path, identity)
        } else {
            Entry::Other
        })
    }
    fn link(&mut self, handle: &PathBuf, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
        self.boundary(budget)?;
        if self.changed {
            return Ok(b"/changed".to_vec());
        }
        self.nodes
            .get(handle)
            .and_then(|node| node.link.clone())
            .ok_or(ProbeError::new(Reason::SpecialIndirection))
    }
    fn accessible(&mut self, handle: &PathBuf, budget: &mut Budget) -> ProbeResult<()> {
        self.boundary(budget)?;
        if self.nodes.get(handle).is_some_and(|node| node.accessible) {
            Ok(())
        } else {
            Err(ProbeError {
                reason: Reason::Inaccessible,
                code: Some(libc::EACCES),
            })
        }
    }
    fn identity(&mut self, handle: &PathBuf, budget: &mut Budget) -> ProbeResult<ObjectIdentity> {
        self.boundary(budget)?;
        Ok(self.nodes[handle].identity)
    }
}

impl Kernel for Fake {
    fn backing_hash(&mut self, budget: &mut Budget) -> ProbeResult<String> {
        self.boundary(budget)?;
        Ok(evidence::hash(
            b"fake-backing",
            b"bounded-scripted-topology",
        ))
    }
    fn correlate(
        &mut self,
        snapshot: &Snapshot,
        resolved: &Resolved<PathBuf>,
        budget: &mut Budget,
    ) -> ProbeResult<(Mount, FilesystemClass)> {
        self.boundary(budget)?;
        let id = self.nodes[&resolved.handle].mount;
        Ok((
            snapshot.mounts[&id].clone(),
            if id == 2 {
                FilesystemClass::Memory
            } else {
                FilesystemClass::Ext
            },
        ))
    }
    fn backing(
        &mut self,
        handle: &PathBuf,
        _mount: &Mount,
        _filesystem: FilesystemClass,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        self.boundary(budget)?;
        let result = self.backing[&self.nodes[handle].mount];
        self.changed = self.race_after_backing;
        Ok(result)
    }
    fn mount_identity(&mut self, handle: &PathBuf, budget: &mut Budget) -> ProbeResult<u64> {
        self.boundary(budget)?;
        Ok(self.nodes[handle].mount)
    }
    fn recheck(&mut self, _initial: &Snapshot, budget: &mut Budget) -> ProbeResult<()> {
        self.boundary(budget)?;
        if self.execution_race {
            Err(ProbeError::new(Reason::Changed))
        } else {
            Ok(())
        }
    }
}

fn requirement(kind: ConstraintKind, id: u128) -> StorageRequirement {
    StorageRequirement {
        identity: PolicyIdentity {
            id: Uuid::from_u128(id),
            key: "rust.build.storage.workstation".to_owned(),
            revision: 3,
            project_hash: evidence::hash(b"project", b"general"),
        },
        kind,
        failure: None,
    }
}
fn candidate(role: CandidateRole, path: &str) -> Candidate {
    Candidate::new(0, role, OsStr::new(path)).unwrap()
}
fn result(fake: Fake, path: &str) -> Report {
    fake.report(
        ConstraintKind::PersistentTarget,
        &[candidate(CandidateRole::TargetDirectory, path)],
        None,
    )
}

fn proof() -> attestation::StorageExecution {
    serde_json::from_value(serde_json::json!({
        "contract":"isolated-woodpecker-storage-v1", "binding_root":"/disk", "project":"general", "boot_id":"00000000-0000-0000-0000-000000000001", "namespace_device":9, "namespace_inode":10, "root_device":1, "root_inode":1, "isolated_woodpecker":true,
        "mounts":[{"id":2,"root":"/","major":0,"minor":2,"mountpoint":"/tmp","mountpoint_device":2,"mountpoint_inode":3,"filesystem":"tmpfs","provenance":"container-local"}]
    })).unwrap()
}

#[test]
fn host_role_and_backing_matrix() {
    for role in [
        CandidateRole::TargetDirectory,
        CandidateRole::BuildOutput,
        CandidateRole::CompilerTemporary,
        CandidateRole::SourceWorktree,
    ] {
        let kind = if role == CandidateRole::CompilerTemporary {
            ConstraintKind::PersistentCompilerTemporary
        } else {
            ConstraintKind::PersistentTarget
        };
        assert_eq!(
            Fake::new()
                .report(kind, &[candidate(role, "/disk/source")], None)
                .outcome,
            Outcome::Pass
        );
        assert_eq!(
            Fake::new()
                .report(kind, &[candidate(role, "/tmp/target")], None)
                .outcome,
            Outcome::Deny
        );
        let mut fake = Fake::new();
        fake.backing.insert(1, BackingClass::Unknown);
        assert_eq!(
            fake.report(kind, &[candidate(role, "/disk/source")], None)
                .outcome,
            Outcome::Indeterminate
        );
        let mut fake = Fake::new();
        fake.nodes
            .get_mut(Path::new("/disk/source"))
            .unwrap()
            .accessible = false;
        assert_eq!(
            fake.report(kind, &[candidate(role, "/disk/source")], None)
                .outcome,
            Outcome::Indeterminate
        );
    }
}

#[test]
fn ordered_path_walk_and_missing_suffix() {
    let mut fake = Fake::new();
    fake.symlink("/disk/jump", b"/tmp/target");
    fake.symlink("/disk/relative", b"../tmp/target");
    fake.symlink("/tmp/target/back", b"/disk");
    for path in [
        "/disk/jump",
        "/disk/jump/..",
        "/disk/relative/new",
        "/tmp/target/./out",
        "/tmp/target/missing/deeper",
    ] {
        let mut budget = Budget::new();
        let resolved = path::resolve(
            &mut fake,
            &mut budget,
            Path::new("/disk"),
            &candidate(CandidateRole::TargetDirectory, path),
        )
        .unwrap();
        assert!(resolved.effective.starts_with("/tmp"));
        assert_eq!(fake.nodes[&resolved.handle].mount, 2);
        resolved.recheck(&mut fake, &mut budget).unwrap();
    }
    assert_eq!(result(fake, "/tmp/target/back/new").outcome, Outcome::Pass);
    assert_eq!(result(Fake::new(), "source/./..").outcome, Outcome::Pass);
    assert_eq!(
        result(Fake::new(), "/disk/missing/..").assessments[0].reason,
        Reason::MissingParent
    );
    assert_eq!(
        Fake::new()
            .report(
                ConstraintKind::PersistentTarget,
                &[candidate(CandidateRole::SourceWorktree, "/disk/absent")],
                None
            )
            .assessments[0]
            .reason,
        Reason::MissingSource
    );
}

#[test]
fn dangling_loop_errors_and_special_objects_do_not_inherit() {
    let mut fake = Fake::new();
    fake.symlink("/disk/dangling", b"/disk/absent");
    assert_eq!(
        result(fake, "/disk/dangling/out").assessments[0].reason,
        Reason::DanglingSymlink
    );
    let mut fake = Fake::new();
    fake.symlink("/disk/loop", b"loop");
    assert_eq!(
        result(fake, "/disk/loop").assessments[0].reason,
        Reason::Limit
    );
    for code in [libc::EACCES, libc::ELOOP, libc::ENOTDIR, libc::EIO] {
        let mut fake = Fake::new();
        fake.injected = Some(code);
        let report = result(fake, "/disk/out");
        assert_eq!(report.outcome, Outcome::Indeterminate);
        assert_eq!(
            report.assessments[0].evidence.as_ref().unwrap().os_code,
            Some(code)
        );
    }
    for mode in [libc::S_IFREG, libc::S_IFIFO, libc::S_IFBLK, libc::S_IFCHR] {
        let mut fake = Fake::new();
        fake.nodes
            .get_mut(Path::new("/disk/source"))
            .unwrap()
            .identity
            .mode = mode;
        assert_eq!(
            result(fake, "/disk/source").assessments[0].reason,
            Reason::NotDirectory
        );
    }
}

#[test]
fn non_utf8_and_path_boundaries_are_not_lossy() {
    let path = OsString::from_vec(b"/disk/SENTINEL_SECRET_\xff".to_vec());
    let candidate = Candidate::new(0, CandidateRole::BuildOutput, path).unwrap();
    let report = Fake::new().report(ConstraintKind::PersistentTarget, &[candidate], None);
    assert_eq!(report.outcome, Outcome::Pass);
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("SENTINEL"));
    assert!(!format!("{report:?}").contains("SENTINEL"));
    assert!(!encoded.contains("SECRET_MOUNT"));
    assert!(
        Candidate::new(
            63,
            CandidateRole::BuildOutput,
            OsString::from_vec(vec![b'a'; 4096])
        )
        .is_ok()
    );
    assert!(Candidate::new(64, CandidateRole::BuildOutput, "x").is_err());
    assert!(
        Candidate::new(
            0,
            CandidateRole::BuildOutput,
            OsString::from_vec(vec![b'a'; 4097])
        )
        .is_err()
    );
    assert!(Candidate::new(0, CandidateRole::BuildOutput, OsString::from_vec(vec![0])).is_err());
}

#[test]
fn namespace_mount_object_and_link_races_invalidate() {
    let mut fake = Fake::new();
    fake.race_after_backing = true;
    assert_eq!(result(fake, "/disk/source").outcome, Outcome::Indeterminate);
    let mut fake = Fake::new();
    fake.symlink("/disk/alias", b"source");
    fake.race_after_backing = true;
    assert_eq!(result(fake, "/disk/alias").outcome, Outcome::Indeterminate);
    let mut fake = Fake::new();
    fake.execution_race = true;
    assert_eq!(result(fake, "/disk/source").outcome, Outcome::Indeterminate);
}

#[test]
fn container_exception_requires_exact_proof_and_location() {
    assert_eq!(result(Fake::new(), "/tmp/target").outcome, Outcome::Deny);
    for role in [CandidateRole::TargetDirectory, CandidateRole::BuildOutput] {
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(role, "/tmp/target")],
                    Some(proof())
                )
                .outcome,
            Outcome::Pass
        );
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(role, "/tmp/target")],
                    None
                )
                .outcome,
            Outcome::Indeterminate
        );
    }
    for path in ["/tmp/target-other", "/disk", "/tmp/target/../outside"] {
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(CandidateRole::BuildOutput, path)],
                    Some(proof())
                )
                .outcome,
            Outcome::Deny
        );
    }
    let mut fake = Fake::new();
    fake.symlink("/tmp/target/escape", b"/disk");
    assert_eq!(
        fake.report(
            ConstraintKind::ContainerTarget,
            &[candidate(CandidateRole::BuildOutput, "/tmp/target/escape")],
            Some(proof())
        )
        .outcome,
        Outcome::Deny
    );
    let mut fake = Fake::new();
    fake.symlink("/disk/alias", b"/tmp/target");
    assert_eq!(
        fake.report(
            ConstraintKind::ContainerTarget,
            &[candidate(CandidateRole::TargetDirectory, "/disk/alias")],
            Some(proof())
        )
        .outcome,
        Outcome::Deny
    );
    for role in [
        CandidateRole::CompilerTemporary,
        CandidateRole::SourceWorktree,
    ] {
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(role, "/disk/source")],
                    None
                )
                .outcome,
            Outcome::NotApplicable
        );
    }
    assert_eq!(
        Fake::new()
            .report(
                ConstraintKind::ContainerTarget,
                &[candidate(CandidateRole::BuildOutput, "/tmp/target/nested")],
                Some(proof())
            )
            .outcome,
        Outcome::Indeterminate
    );
}

#[test]
fn each_execution_and_mount_pin_is_checked() {
    let base = serde_json::to_value(proof()).unwrap();
    for field in [
        "namespace_device",
        "namespace_inode",
        "root_device",
        "root_inode",
    ] {
        let mut altered = base.clone();
        altered[field] = serde_json::json!(999);
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(CandidateRole::TargetDirectory, "/tmp/target")],
                    Some(serde_json::from_value(altered).unwrap())
                )
                .outcome,
            Outcome::Deny
        );
    }
    let mut altered = base.clone();
    altered["boot_id"] = serde_json::json!("00000000-0000-0000-0000-000000000002");
    assert_eq!(
        Fake::new()
            .report(
                ConstraintKind::ContainerTarget,
                &[candidate(CandidateRole::TargetDirectory, "/tmp/target")],
                Some(serde_json::from_value(altered).unwrap())
            )
            .outcome,
        Outcome::Deny
    );
    for (field, value) in [
        ("root", serde_json::json!("/different")),
        ("major", serde_json::json!(1)),
        ("minor", serde_json::json!(9)),
        ("mountpoint_inode", serde_json::json!(999)),
        ("mountpoint_device", serde_json::json!(999)),
        ("mountpoint", serde_json::json!("/tmp/target")),
        ("filesystem", serde_json::json!("ext4")),
    ] {
        let mut altered = base.clone();
        altered["mounts"][0][field] = value;
        assert_eq!(
            Fake::new()
                .report(
                    ConstraintKind::ContainerTarget,
                    &[candidate(CandidateRole::TargetDirectory, "/tmp/target")],
                    Some(serde_json::from_value(altered).unwrap())
                )
                .outcome,
            Outcome::Deny
        );
    }
}

#[test]
fn aggregation_coverage_bounds_and_audit_envelope() {
    let candidates = [
        Candidate::new(0, CandidateRole::BuildOutput, "/disk").unwrap(),
        Candidate::new(1, CandidateRole::SourceWorktree, "/tmp/target").unwrap(),
        Candidate::new(2, CandidateRole::CompilerTemporary, "/disk").unwrap(),
    ];
    let report = Fake::new().report(ConstraintKind::PersistentTarget, &candidates, None);
    assert_eq!(report.outcome, Outcome::Deny);
    assert_eq!(report.assessments.len(), 3);
    assert_eq!(report.assessments[2].outcome, Outcome::NotApplicable);
    let projection = report.audit_projection().unwrap();
    assert_eq!(projection.counts, [1, 1, 0, 1]);
    let envelope = serde_json::json!({ "storage": projection, "event_id": Uuid::nil(), "attempt": Uuid::nil(), "adapter":"claude-v1", "category":"mutation", "session_hash":"f".repeat(64), "binding_hash":"f".repeat(64), "epoch":u64::MAX, "generation":Uuid::nil(), "intent":"denied", "reason":"storage", "event_kind":"child_pre_tool", "parent_hash":"f".repeat(64), "child_hash":"f".repeat(64), "version":3 });
    assert!(serde_json::to_vec(&envelope).unwrap().len() <= 2048);
    assert_eq!(
        Fake::new()
            .report(ConstraintKind::PersistentTarget, &[], None)
            .outcome,
        Outcome::Indeterminate
    );
    let candidates = (0..64)
        .map(|index| Candidate::new(index, CandidateRole::BuildOutput, "/disk").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        Fake::new()
            .report(ConstraintKind::PersistentTarget, &candidates, None)
            .outcome,
        Outcome::Pass
    );
    let mut over = candidates;
    over.push(candidate(CandidateRole::BuildOutput, "/disk"));
    assert_eq!(
        Fake::new()
            .report(ConstraintKind::PersistentTarget, &over, None)
            .outcome,
        Outcome::Indeterminate
    );
    let mut budget = Budget::new();
    budget.operations = MAX_OPERATIONS;
    assert_eq!(budget.tick().unwrap_err().reason, Reason::Limit);
    let mut budget = Budget::new();
    budget.start -= Duration::from_secs(5);
    assert_eq!(budget.tick().unwrap_err().reason, Reason::Limit);
}

#[test]
fn exact_path_link_limits_and_counting_provider() {
    let mut fake = Fake::new();
    for index in 0..40 {
        let target = if index == 39 {
            b"source".to_vec()
        } else {
            format!("link{}", index + 1).into_bytes()
        };
        fake.symlink(&format!("/disk/link{index}"), &target);
    }
    let mut budget = Budget::new();
    let resolved = path::resolve(
        &mut fake,
        &mut budget,
        Path::new("/disk"),
        &candidate(CandidateRole::BuildOutput, "link0"),
    )
    .unwrap();
    assert_eq!(resolved.effective, Path::new("/disk/source"));
    assert!(fake.calls < 1000);
    fake.symlink("/disk/start", b"link0");
    assert_eq!(
        result(fake, "/disk/start").assessments[0].reason,
        Reason::Limit
    );
    for (steps, outcome) in [(256, Outcome::Pass), (257, Outcome::Indeterminate)] {
        let path = format!("/{}", vec!["."; steps].join("/"));
        assert_eq!(result(Fake::new(), &path).outcome, outcome);
    }
    assert_eq!(
        result(Fake::new(), "/tmp/target/nested").outcome,
        Outcome::Pass
    );
}

#[test]
fn overflow_retains_complete_counts_hash_and_decisive_policy() {
    let candidates = (0..64)
        .map(|index| {
            Candidate::new(
                index,
                CandidateRole::BuildOutput,
                if index == 63 { "/tmp/target" } else { "/disk" },
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let report = Engine {
        cwd: PathBuf::from("/disk"),
        profile: Some("workstation-host".to_owned()),
        scope_hash: "scope".to_owned(),
        pack_digest: "digest".to_owned(),
        requirements: (1..=32)
            .map(|id| requirement(ConstraintKind::PersistentTarget, id))
            .collect(),
        provider: Fake::new(),
        initial: Snapshot::fixture(TABLE),
        attestation: None,
        budget: Budget::new(),
    }
    .evaluate(&candidates);
    assert_eq!(report.outcome, Outcome::Indeterminate);
    assert_eq!(report.assessments.len(), 1);
    assert_eq!(report.assessments[0].reason, Reason::ReportOverflow);
    assert_eq!(report.assessments[0].candidate_index, Some(63));
    assert_eq!(report.counts, [0, 2016, 0, 32]);
    assert!(report.assessments[0].policy.is_some());
    assert_eq!(report.complete_assessment_hash.len(), 64);
    assert!(serde_json::to_vec(&report).unwrap().len() < MAX_REPORT_BYTES);
    assert_eq!(report.audit_projection().unwrap().counts[3], 32);
}

#[test]
fn ordering_and_reports_are_repeatable() {
    let first = [
        Candidate::new(2, CandidateRole::BuildOutput, "/tmp/target").unwrap(),
        Candidate::new(1, CandidateRole::BuildOutput, "/disk").unwrap(),
    ];
    let second = [
        Candidate::new(1, CandidateRole::BuildOutput, "/disk").unwrap(),
        Candidate::new(2, CandidateRole::BuildOutput, "/tmp/target").unwrap(),
    ];
    assert_eq!(
        Fake::new().report(ConstraintKind::PersistentTarget, &first, None),
        Fake::new().report(ConstraintKind::PersistentTarget, &second, None)
    );
}
