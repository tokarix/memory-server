//! Atomic, manifest-driven conversion of the historical Rust storage Rules.

use std::collections::BTreeSet;

use chrono::Utc;
use memory_common::policy::{
    CanonicalPolicySet, DeliveryClass, PolicyMetadata, PolicySelectors, PolicyState, PolicyWrite,
    ResolutionContext,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::Error;
use crate::model::{Category, Memory, MemorySummary};
use crate::policy;
use crate::policy_resolution::{self, ResolutionRequest};

/// A source Rule pinned to its exact persisted pre-conversion state.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRule {
    /// Existing legacy Rule UUID.
    pub id: Uuid,
    /// Full, timezone-qualified persisted read token.
    pub expected_updated_at: String,
    /// Exact existing content, including newlines.
    pub expected_content: String,
    /// Exact existing summary.
    pub expected_summary: String,
    /// Exact existing tags in their stored order.
    pub expected_tags: Vec<String>,
}

/// One exact adoption of a legacy source.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Adoption {
    /// Source and read preconditions.
    pub source: SourceRule,
    /// Identity assigned without changing source content.
    pub policy: PolicyWrite,
}

/// A new immutable successor or independent root, with a fixed UUID.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    /// Fixed UUID, also used for idempotent replay verification.
    pub id: Uuid,
    /// Exact normative content.
    pub content: String,
    /// Search summary.
    pub summary: String,
    /// Stored tags.
    pub tags: Vec<String>,
    /// Classified policy metadata.
    pub policy: PolicyWrite,
}

/// Explicit source-to-target mapping for the known host/container conflict.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionManifest {
    /// Manifest schema version; currently 1.
    pub version: u32,
    /// Project used for host and container resolution previews.
    pub preview_project: String,
    /// First host source, adopted as revision 1.
    pub host_first: Adoption,
    /// Second host source, adopted as revision 2.
    pub host_second: Adoption,
    /// Curated active host revision 3.
    pub host_active: Publication,
    /// Mixed Woodpecker source, adopted as historical guidance revision 1.
    pub guidance_source: Adoption,
    /// Storage-separated active Woodpecker guidance revision 2.
    pub guidance_active: Publication,
    /// Independent mandatory container storage root.
    pub container_active: Publication,
}

/// The two complete policy previews returned by dry-run or apply.
#[derive(Clone, Debug, Serialize)]
pub struct ConversionPreview {
    /// Whether the exact manifest result was already present.
    pub already_applied: bool,
    /// Fixed source-to-target changes in transaction order.
    pub changes: Vec<serde_json::Value>,
    /// Effective host policies.
    pub workstation_host: CanonicalPolicySet,
    /// Effective isolated Woodpecker policies.
    pub woodpecker_container: CanonicalPolicySet,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Policy {
        code: "policy_conversion_invalid_manifest".to_owned(),
        message: message.into(),
        details: json!({"action": "Review the pinned source records and manifest"}),
        conflict: false,
    }
}

fn stale(message: impl Into<String>, id: Uuid) -> Error {
    Error::Policy {
        code: "policy_conversion_stale".to_owned(),
        message: message.into(),
        details: json!({"id": id, "action": "Re-read the record; do not refresh the manifest automatically"}),
        conflict: true,
    }
}

impl ConversionManifest {
    /// Validate exact chain, domain, and setting contracts before embedding.
    ///
    /// # Errors
    /// Returns a typed error if this is not the reviewed storage conversion.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != 1 || self.preview_project.is_empty() {
            return Err(invalid(
                "version must be 1 and preview_project must be nonempty",
            ));
        }
        let sources = [&self.host_first, &self.host_second, &self.guidance_source];
        let outputs = [
            &self.host_active,
            &self.guidance_active,
            &self.container_active,
        ];
        let mut ids = BTreeSet::new();
        for adoption in sources {
            adoption.policy.validate().map_err(invalid)?;
            policy::parse_expected_updated_at(
                &adoption.source.expected_updated_at,
                &adoption.policy,
            )?;
            if !ids.insert(adoption.source.id)
                || adoption.source.expected_content.is_empty()
                || adoption.source.expected_summary.is_empty()
            {
                return Err(invalid(
                    "source IDs must be unique and source text nonempty",
                ));
            }
        }
        for publication in outputs {
            publication.policy.validate().map_err(invalid)?;
            if !ids.insert(publication.id)
                || publication.content.is_empty()
                || publication.summary.is_empty()
            {
                return Err(invalid("publication IDs must be unique and text nonempty"));
            }
        }
        let host_key = "rust.build.storage.workstation";
        let guidance_key = "rust.ci.woodpecker";
        let container_key = "rust.build.storage.woodpecker";
        if self.host_first.policy.policy_key != host_key
            || self.host_second.policy.policy_key != host_key
            || self.host_active.policy.policy_key != host_key
            || self.guidance_source.policy.policy_key != guidance_key
            || self.guidance_active.policy.policy_key != guidance_key
            || self.container_active.policy.policy_key != container_key
        {
            return Err(invalid(
                "policy keys do not match the reviewed storage identities",
            ));
        }
        let chain = [
            (&self.host_first.policy, 1, None),
            (&self.host_second.policy, 2, Some(self.host_first.source.id)),
            (
                &self.host_active.policy,
                3,
                Some(self.host_second.source.id),
            ),
            (&self.guidance_source.policy, 1, None),
            (
                &self.guidance_active.policy,
                2,
                Some(self.guidance_source.source.id),
            ),
            (&self.container_active.policy, 1, None),
        ];
        if chain.iter().any(|(policy, revision, predecessor)| {
            policy.revision != *revision || policy.supersedes != *predecessor
        }) {
            return Err(invalid(
                "revision numbers or explicit predecessor chain differ",
            ));
        }
        for policy in [
            &self.host_first.policy,
            &self.host_second.policy,
            &self.host_active.policy,
        ] {
            if policy.delivery_class != DeliveryClass::Mandatory
                || !single(policy.selectors.profile.as_ref(), "workstation-host")
                || !single(policy.selectors.language.as_ref(), "rust")
                || policy.selectors.phase.is_some()
                || policy.selectors.tool.is_some()
            {
                return Err(invalid(
                    "host revisions need the host/Rust mandatory domain",
                ));
            }
        }
        let host_values = &self.host_active.policy.values;
        if host_values
            .get("rust.build.target_storage")
            .map(String::as_str)
            != Some("persistent-disk")
            || host_values
                .get("rust.build.compiler_tmp_storage")
                .map(String::as_str)
                != Some("persistent-disk")
            || host_values.len() != 2
        {
            return Err(invalid(
                "active host storage values differ from the reviewed contract",
            ));
        }
        let container = &self.container_active.policy;
        if container.delivery_class != DeliveryClass::Mandatory
            || !single(container.selectors.profile.as_ref(), "woodpecker-container")
            || !single(container.selectors.language.as_ref(), "rust")
            || container.selectors.phase.is_some()
            || container.selectors.tool.is_some()
            || container.values.len() != 1
            || container
                .values
                .get("rust.build.target_storage")
                .map(String::as_str)
                != Some("container-local-tmp")
        {
            return Err(invalid("container storage domain or values differ"));
        }
        if [&self.guidance_source.policy, &self.guidance_active.policy]
            .into_iter()
            .any(|policy| {
                policy.delivery_class != DeliveryClass::Contextual
                    || !policy.values.is_empty()
                    || policy.selectors != PolicySelectors::default()
            })
            || self.guidance_active.content.contains("/tmp/target")
        {
            return Err(invalid(
                "active guidance must be contextual and storage-neutral",
            ));
        }
        Ok(())
    }

    fn sources(&self) -> [&Adoption; 3] {
        [&self.host_first, &self.host_second, &self.guidance_source]
    }

    fn outputs(&self) -> [&Publication; 3] {
        [
            &self.host_active,
            &self.guidance_active,
            &self.container_active,
        ]
    }

    fn changes(&self) -> Vec<serde_json::Value> {
        self.sources()
            .into_iter()
            .map(|adoption| {
                json!({"operation": "adopt", "source": adoption.source.id,
                       "key": adoption.policy.policy_key, "revision": adoption.policy.revision,
                       "expected_updated_at": adoption.source.expected_updated_at})
            })
            .chain(self.outputs().into_iter().map(|publication| {
                json!({"operation": "publish", "id": publication.id,
                       "key": publication.policy.policy_key,
                       "revision": publication.policy.revision,
                       "supersedes": publication.policy.supersedes})
            }))
            .collect()
    }
}

fn single(values: Option<&BTreeSet<String>>, expected: &str) -> bool {
    values.is_some_and(|set| set.len() == 1 && set.contains(expected))
}

fn metadata(write: &PolicyWrite, state: PolicyState) -> PolicyMetadata {
    PolicyMetadata {
        policy_key: write.policy_key.clone(),
        revision: write.revision,
        delivery_class: write.delivery_class,
        state,
        supersedes: write.supersedes,
        selectors: write.selectors.clone(),
        values: write.values.clone(),
    }
}

fn check_source(rule: &MemorySummary, adoption: &Adoption, replay: bool) -> Result<(), Error> {
    let source = &adoption.source;
    if rule.id != source.id
        || rule.project != "general"
        || rule.category != Category::Rule
        || rule.content != source.expected_content
        || rule.summary != source.expected_summary
        || rule.tags != source.expected_tags
    {
        return Err(stale(
            "source Rule content or classification changed",
            source.id,
        ));
    }
    if replay {
        if rule.policy.as_ref() != Some(&metadata(&adoption.policy, PolicyState::Superseded)) {
            return Err(stale(
                "adopted source does not match the manifest",
                source.id,
            ));
        }
    } else {
        let expected =
            policy::parse_expected_updated_at(&source.expected_updated_at, &adoption.policy)?;
        if rule.policy.is_some() || rule.updated_at != expected {
            return Err(stale(
                "source Rule is no longer the exact legacy revision",
                source.id,
            ));
        }
    }
    Ok(())
}

fn check_output(rule: &MemorySummary, publication: &Publication) -> Result<(), Error> {
    if rule.id != publication.id
        || rule.project != "general"
        || rule.category != Category::Rule
        || rule.content != publication.content
        || rule.summary != publication.summary
        || rule.tags != publication.tags
        || rule.policy.as_ref() != Some(&metadata(&publication.policy, PolicyState::Active))
    {
        return Err(stale(
            "published target does not match the manifest",
            publication.id,
        ));
    }
    Ok(())
}

fn proposed_snapshot(
    mut candidates: Vec<MemorySummary>,
    manifest: &ConversionManifest,
) -> Vec<MemorySummary> {
    for adoption in manifest.sources() {
        if let Some(source) = candidates
            .iter_mut()
            .find(|rule| rule.id == adoption.source.id)
        {
            source.policy = Some(metadata(&adoption.policy, PolicyState::Superseded));
        }
    }
    let now = Utc::now();
    for publication in manifest.outputs() {
        candidates.push(MemorySummary {
            id: publication.id,
            policy: Some(metadata(&publication.policy, PolicyState::Active)),
            category: Category::Rule,
            content: publication.content.clone(),
            created_at: now,
            project: "general".to_owned(),
            summary: publication.summary.clone(),
            tags: publication.tags.clone(),
            updated_at: now,
        });
    }
    candidates
}

fn resolve_preview(
    candidates: &[MemorySummary],
    manifest: &ConversionManifest,
    already_applied: bool,
) -> Result<ConversionPreview, Error> {
    let resolve_for = |profile: &str| {
        policy_resolution::resolve(
            candidates.to_vec(),
            &ResolutionRequest {
                project: manifest.preview_project.clone(),
                include_general: true,
                shadow_general: true,
                tags: None,
                context: ResolutionContext {
                    profile: Some(profile.to_owned()),
                    language: Some(BTreeSet::from(["rust".to_owned()])),
                    ..ResolutionContext::default()
                },
            },
        )
        .map(|rules| rules.canonical)
    };
    let workstation_host = resolve_for("workstation-host")?;
    let woodpecker_container = resolve_for("woodpecker-container")?;
    Ok(ConversionPreview {
        already_applied,
        changes: manifest.changes(),
        workstation_host,
        woodpecker_container,
    })
}

/// Inspect the exact source state and preview both effective policy sets without writes.
///
/// # Errors
/// Returns a stale-source or resolution conflict if conversion is unsafe.
pub async fn dry_run(
    pool: &PgPool,
    manifest: &ConversionManifest,
) -> Result<ConversionPreview, Error> {
    manifest.validate()?;
    let mut sources = Vec::new();
    for adoption in manifest.sources() {
        let rule = db::get(pool, adoption.source.id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| stale("source Rule disappeared", adoption.source.id))?;
        sources.push(rule);
    }
    let mut outputs = Vec::new();
    for publication in manifest.outputs() {
        outputs.push(db::get(pool, publication.id).await.map_err(Error::from)?);
    }
    let replay = outputs.iter().all(Option::is_some);
    if !replay && outputs.iter().any(Option::is_some) {
        return Err(invalid("only part of the fixed output set exists"));
    }
    for (rule, adoption) in sources.iter().zip(manifest.sources()) {
        check_source(rule, adoption, replay)?;
    }
    if replay {
        for (rule, publication) in outputs.iter().zip(manifest.outputs()) {
            if let Some(rule) = rule {
                check_output(rule, publication)?;
            }
        }
    }
    let candidates = db::load_rule_candidates(pool, &manifest.preview_project)
        .await
        .map_err(Error::from)?;
    let candidates = if replay {
        candidates
    } else {
        proposed_snapshot(candidates, manifest)
    };
    resolve_preview(&candidates, manifest, replay)
}

fn prepared_memory(publication: &Publication, embedding: Vec<f32>) -> Memory {
    let now = Utc::now();
    Memory {
        id: publication.id,
        policy: None,
        category: Category::Rule,
        content: publication.content.clone(),
        created_at: now,
        embedding,
        project: "general".to_owned(),
        summary: publication.summary.clone(),
        tags: publication.tags.clone(),
        updated_at: now,
    }
}

/// Atomically adopt all sources, publish all successors, resolve, and commit.
///
/// Embeddings must be prepared before calling this function. The three vectors
/// correspond to host, guidance, and container publications in that order.
///
/// # Errors
/// Returns a stale precondition or policy conflict and rolls back all writes.
pub async fn apply(
    pool: &PgPool,
    manifest: &ConversionManifest,
    embeddings: [Vec<f32>; 3],
) -> Result<ConversionPreview, Error> {
    manifest.validate()?;
    let mut transaction = pool.begin().await.map_err(Error::from)?;
    for key in [
        "rust.build.storage.woodpecker",
        "rust.build.storage.workstation",
        "rust.ci.woodpecker",
    ] {
        policy::acquire_identity_lock(&mut transaction, "general", key).await?;
    }
    let source_ids: Vec<_> = manifest
        .sources()
        .into_iter()
        .map(|adoption| adoption.source.id)
        .collect();
    policy::lock_rows(&mut transaction, &source_ids).await?;
    let mut sources = Vec::new();
    for adoption in manifest.sources() {
        sources.push(
            db::get_in_transaction(&mut transaction, adoption.source.id)
                .await
                .map_err(Error::from)?
                .ok_or_else(|| stale("source Rule disappeared", adoption.source.id))?,
        );
    }
    let mut outputs = Vec::new();
    for publication in manifest.outputs() {
        outputs.push(
            db::get_in_transaction(&mut transaction, publication.id)
                .await
                .map_err(Error::from)?,
        );
    }
    let replay = outputs.iter().all(Option::is_some);
    if !replay && outputs.iter().any(Option::is_some) {
        return Err(invalid("only part of the fixed output set exists"));
    }
    for (rule, adoption) in sources.iter().zip(manifest.sources()) {
        check_source(rule, adoption, replay)?;
    }
    if replay {
        for (rule, publication) in outputs.iter().zip(manifest.outputs()) {
            if let Some(rule) = rule {
                check_output(rule, publication)?;
            }
        }
    } else {
        for adoption in manifest.sources() {
            let expected = policy::parse_expected_updated_at(
                &adoption.source.expected_updated_at,
                &adoption.policy,
            )?;
            policy::adopt_in_transaction(
                &mut transaction,
                adoption.source.id,
                "general",
                &adoption.policy,
                expected,
            )
            .await?;
        }
        for (publication, embedding) in manifest.outputs().into_iter().zip(embeddings) {
            let memory = prepared_memory(publication, embedding);
            policy::publish_in_transaction(&mut transaction, &memory, &publication.policy).await?;
        }
    }
    let candidates =
        db::load_rule_candidates_in_transaction(&mut transaction, &manifest.preview_project)
            .await
            .map_err(Error::from)?;
    let preview = resolve_preview(&candidates, manifest, replay)?;
    transaction.commit().await.map_err(Error::from)?;
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use sqlx::PgPool;
    use tokio::time::timeout;

    use crate::db;
    use crate::model::{Category, Memory};

    use super::{ConversionManifest, apply, dry_run};

    fn manifest() -> ConversionManifest {
        serde_json::from_str(include_str!(
            "../../docs/manifests/rust-storage-2026-09-26.json"
        ))
        .unwrap()
    }

    async fn seed(pool: &PgPool, manifest: &ConversionManifest) {
        for adoption in manifest.sources() {
            let source = &adoption.source;
            let updated_at = DateTime::parse_from_rfc3339(&source.expected_updated_at)
                .unwrap()
                .with_timezone(&Utc);
            db::insert(
                pool,
                &Memory {
                    id: source.id,
                    policy: None,
                    category: Category::Rule,
                    content: source.expected_content.clone(),
                    created_at: updated_at,
                    embedding: vec![0.0; 1024],
                    project: "general".to_owned(),
                    summary: source.expected_summary.clone(),
                    tags: source.expected_tags.clone(),
                    updated_at,
                },
            )
            .await
            .unwrap();
        }
    }

    fn embeddings() -> [Vec<f32>; 3] {
        [vec![0.0; 1024], vec![0.0; 1024], vec![0.0; 1024]]
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn dry_run_apply_and_exact_replay_preserve_history(pool: PgPool) {
        let manifest = manifest();
        seed(&pool, &manifest).await;
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories")
            .fetch_one(&pool)
            .await
            .unwrap();
        let preview = dry_run(&pool, &manifest).await.unwrap();
        assert!(!preview.already_applied);
        assert_eq!(preview.changes.len(), 6);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories")
                .fetch_one(&pool)
                .await
                .unwrap(),
            before
        );
        for adoption in manifest.sources() {
            assert!(
                db::get(&pool, adoption.source.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .policy
                    .is_none()
            );
        }
        let applied = apply(&pool, &manifest, embeddings()).await.unwrap();
        assert!(!applied.already_applied);
        let host = &applied.workstation_host.mandatory;
        let container = &applied.woodpecker_container.mandatory;
        assert!(host.iter().any(|rule| rule.id == manifest.host_active.id));
        assert!(
            !host
                .iter()
                .any(|rule| rule.id == manifest.container_active.id)
        );
        assert!(
            applied
                .workstation_host
                .effective
                .iter()
                .all(|rule| !rule.content.contains("/tmp/target"))
        );
        assert_eq!(
            host.iter()
                .find(|rule| rule.id == manifest.host_active.id)
                .unwrap()
                .values
                .get("rust.build.compiler_tmp_storage")
                .map(String::as_str),
            Some("persistent-disk")
        );
        assert!(
            container
                .iter()
                .any(|rule| rule.id == manifest.container_active.id)
        );
        assert_eq!(
            container
                .iter()
                .find(|rule| rule.id == manifest.container_active.id)
                .unwrap()
                .values
                .get("rust.build.target_storage")
                .map(String::as_str),
            Some("container-local-tmp")
        );
        assert!(
            !container
                .iter()
                .any(|rule| rule.id == manifest.host_active.id)
        );
        for effective in [
            &applied.workstation_host.effective,
            &applied.woodpecker_container.effective,
        ] {
            assert!(effective.iter().all(|rule| {
                rule.id != manifest.guidance_active.id || !rule.content.contains("/tmp/target")
            }));
            assert!(effective.iter().all(|rule| {
                !manifest
                    .sources()
                    .iter()
                    .any(|source| source.source.id == rule.id)
            }));
        }
        for adoption in manifest.sources() {
            let stored = db::get(&pool, adoption.source.id).await.unwrap().unwrap();
            assert_eq!(stored.content, adoption.source.expected_content);
            assert_eq!(
                stored.policy.unwrap().state,
                memory_common::policy::PolicyState::Superseded
            );
        }
        let replay = apply(&pool, &manifest, embeddings()).await.unwrap();
        assert!(replay.already_applied);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories")
                .fetch_one(&pool)
                .await
                .unwrap(),
            before + 3
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn stale_source_and_mid_batch_failure_roll_back_every_change(pool: PgPool) {
        let manifest = manifest();
        seed(&pool, &manifest).await;
        let mut wrong_vectors = embeddings();
        wrong_vectors[1] = vec![0.0; 2];
        assert!(apply(&pool, &manifest, wrong_vectors).await.is_err());
        for adoption in manifest.sources() {
            assert!(
                db::get(&pool, adoption.source.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .policy
                    .is_none()
            );
        }
        for publication in manifest.outputs() {
            assert!(db::get(&pool, publication.id).await.unwrap().is_none());
        }
        assert!(
            db::update(
                &pool,
                manifest.host_second.source.id,
                None,
                None,
                Some("intervening edit"),
                None
            )
            .await
            .unwrap()
        );
        assert!(apply(&pool, &manifest, embeddings()).await.is_err());
        for adoption in manifest.sources() {
            assert!(
                db::get(&pool, adoption.source.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .policy
                    .is_none()
            );
        }
        for publication in manifest.outputs() {
            assert!(db::get(&pool, publication.id).await.unwrap().is_none());
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn concurrent_apply_serializes_and_replay_verifies(pool: PgPool) {
        let manifest = manifest();
        seed(&pool, &manifest).await;
        let first_pool = pool.clone();
        let first_manifest = manifest.clone();
        let first =
            tokio::spawn(async move { apply(&first_pool, &first_manifest, embeddings()).await });
        let second_pool = pool.clone();
        let second_manifest = manifest.clone();
        let second =
            tokio::spawn(async move { apply(&second_pool, &second_manifest, embeddings()).await });
        let (first, second) = timeout(Duration::from_secs(20), async {
            (
                first.await.unwrap().unwrap(),
                second.await.unwrap().unwrap(),
            )
        })
        .await
        .unwrap();
        assert_ne!(first.already_applied, second.already_applied);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories")
                .fetch_one(&pool)
                .await
                .unwrap(),
            6
        );
    }
}
