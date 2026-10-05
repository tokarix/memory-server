//! Compact linked evidence, legacy compatibility and corruption fixtures.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{AuditEvent, Record, RustEvidence, RustOutcome, linkage, validate_record};
use crate::config::ClientAdapter;
use crate::installation::Installation;
use crate::pre_tool::ToolCategory;

fn installation(adapter: ClientAdapter) -> (Installation, tempfile::TempDir) {
    let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let directory = tempfile::tempdir_in(base).unwrap();
    let path = directory.path().join("control");
    let id = Installation::initialize(&path, adapter).unwrap();
    (Installation::open(&path, id, adapter).unwrap(), directory)
}

fn record(adapter: ClientAdapter, delegated: bool) -> Record {
    let hash = "f".repeat(64);
    let projection = serde_json::from_value(json!({
        "version":1, "outcome":"indeterminate", "counts":[0,0,4096,0],
        "reason":"conflicting_requirements", "candidate_index":63,
        "policy_id":Uuid::from_u128(u128::MAX), "policy_revision":i64::MAX,
        "policy_hash":hash, "assessment_hash":hash, "evidence_hash":hash,
    }))
    .unwrap();
    let mut record = Record {
        version: 3,
        event_id: Uuid::from_u128(u128::MAX),
        attempt: Uuid::from_u128(u128::MAX),
        adapter,
        category: ToolCategory::Shell,
        session_hash: (!delegated).then(|| hash.clone()),
        binding_hash: Some(hash.clone()),
        epoch: Some(u64::MAX),
        generation: Some(Uuid::from_u128(u128::MAX)),
        intent: "deny".to_owned(),
        reason: "rust_storage_indeterminate".to_owned(),
        event_kind: delegated.then_some(AuditEvent::ChildPreTool),
        parent_hash: delegated.then(|| hash.clone()),
        child_hash: delegated.then(|| hash.clone()),
        rust: Some(RustEvidence {
            analysis: RustOutcome::CompleteBuild,
            candidates: 64,
            contract: 3,
            operation: hash.clone(),
            scope: hash.clone(),
            pack: format!("sha256:{hash}"),
            storage: Some(projection),
        }),
        linkage: None,
    };
    record.linkage = Some(linkage(&record).unwrap());
    record
}

#[test]
fn worst_case_root_and_delegated_evidence_is_bounded_and_redacted() {
    for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
        let (installation, _directory) = installation(adapter);
        for delegated in [false, true] {
            let record = record(adapter, delegated);
            validate_record(&record, &installation).unwrap();
            let encoded = serde_json::to_vec(&record).unwrap();
            assert!(encoded.len() <= 2048, "{} bytes", encoded.len());
            let decoded: Record = serde_json::from_slice(&encoded).unwrap();
            validate_record(&decoded, &installation).unwrap();
            let text = String::from_utf8(encoded).unwrap();
            assert!(!text.contains("SENTINEL_SECRET"));
            assert!(!text.contains('/'));
        }
    }
}

#[test]
fn v3_corruption_and_unknown_fields_never_become_valid_decisions() {
    let (installation, _directory) = installation(ClientAdapter::ClaudeV1);
    let original = serde_json::to_value(record(ClientAdapter::ClaudeV1, true)).unwrap();
    for (pointer, bad) in [
        ("/version", json!(4)),
        ("/attempt", json!(Uuid::nil())),
        ("/epoch", json!(0)),
        ("/generation", json!(Uuid::nil())),
        ("/child_hash", json!("a".repeat(64))),
        ("/rust/contract", json!(4)),
        ("/rust/candidates", json!(65)),
        ("/rust/operation", json!("SENTINEL_SECRET")),
        ("/rust/scope", json!("a".repeat(64))),
        ("/rust/pack", json!("sha256:secret")),
        ("/rust/analysis", json!("audited_read_only")),
        ("/rust/storage/counts", json!([0, 0, 4097, 0])),
        ("/rust/storage/version", json!(2)),
        ("/rust/storage/candidate_index", json!(64)),
        ("/rust/storage/policy_revision", json!(0)),
        ("/rust/storage/policy_hash", Value::Null),
        ("/reason", json!("rust_storage_denied")),
        ("/intent", json!("neutral")),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = bad;
        let mut decoded: Record = serde_json::from_value(changed).unwrap();
        assert!(
            validate_record(&decoded, &installation).is_err(),
            "{pointer}"
        );
        // Recomputing a digest cannot repair invalid field/decision semantics.
        if !matches!(pointer, "/child_hash" | "/rust/scope") {
            decoded.linkage = Some(linkage(&decoded).unwrap());
            assert!(
                validate_record(&decoded, &installation).is_err(),
                "{pointer}"
            );
        }
    }
    for pointer in ["", "/rust", "/rust/storage"] {
        let mut changed = original.clone();
        changed
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("opaque".to_owned(), json!("SENTINEL_SECRET"));
        assert!(serde_json::from_value::<Record>(changed).is_err());
    }
}

#[test]
fn legacy_versions_remain_valid_without_accepting_new_fields() {
    let (installation, _directory) = installation(ClientAdapter::ClaudeV1);
    for delegated in [false, true] {
        let mut legacy = record(ClientAdapter::ClaudeV1, delegated);
        legacy.version = if delegated { 2 } else { 1 };
        legacy.reason = "checked".to_owned();
        legacy.intent = "neutral".to_owned();
        legacy.rust = None;
        legacy.linkage = None;
        validate_record(&legacy, &installation).unwrap();
        let encoded = serde_json::to_vec(&legacy).unwrap();
        assert!(
            !String::from_utf8(encoded.clone())
                .unwrap()
                .contains("linkage")
        );
        validate_record(&serde_json::from_slice(&encoded).unwrap(), &installation).unwrap();
        legacy.reason = "rust_execution_unsupported".to_owned();
        assert!(validate_record(&legacy, &installation).is_err());
    }
}
