//! Atomic publication and adoption of immutable Rule revisions.

use chrono::{DateTime, Utc};
use memory_common::policy::{DeliveryClass, PolicyWrite};
use pgvector::Vector;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::db;
use crate::error::Error;
use crate::model::{Category, Memory, MemorySummary};

/// Publish a new Rule as the active root or successor of a policy identity.
///
/// # Errors
///
/// Returns a typed policy error for an invalid or stale publication, or a
/// database error if persistence fails.
pub async fn publish_new(
    pool: &PgPool,
    memory: &Memory,
    policy: &PolicyWrite,
) -> Result<MemorySummary, Error> {
    let mut transaction = pool.begin().await.map_err(Error::from)?;
    acquire_identity_lock(&mut transaction, &memory.project, &policy.policy_key).await?;
    publish_in_transaction(&mut transaction, memory, policy).await?;
    transaction.commit().await.map_err(Error::from)?;
    load_published(pool, memory.id).await
}

/// Publish into an existing transaction after the caller holds the identity lock.
///
/// # Errors
/// Returns a typed publication error or a database error.
pub(crate) async fn publish_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    memory: &Memory,
    policy: &PolicyWrite,
) -> Result<(), Error> {
    validate_store(policy, &memory.category)?;
    let head = current_head(transaction, &memory.project, &policy.policy_key).await?;
    if let Some((head_id, _)) = head {
        lock_rows(transaction, &[head_id]).await?;
    }
    validate_publication(transaction, &memory.project, policy, head).await?;
    supersede_head(transaction, head).await?;
    let delivery_class = delivery_class_name(policy.delivery_class);
    sqlx::query(
        "INSERT INTO memories
            (id, category, content, created_at, embedding, project, summary, tags,
             updated_at, policy_key, policy_revision, policy_delivery_class,
             policy_state, policy_supersedes, policy_selectors, policy_values)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp(),
                 $9, $10, $11, 'active', $12, $13, $14)",
    )
    .bind(memory.id)
    .bind(&memory.category)
    .bind(&memory.content)
    .bind(memory.created_at)
    .bind(Vector::from(memory.embedding.clone()))
    .bind(&memory.project)
    .bind(&memory.summary)
    .bind(&memory.tags)
    .bind(&policy.policy_key)
    .bind(policy.revision)
    .bind(delivery_class)
    .bind(policy.supersedes)
    .bind(
        serde_json::to_value(&policy.selectors)
            .map_err(|error| Error::Database(error.to_string()))?,
    )
    .bind(serde_json::to_value(&policy.values).map_err(|error| Error::Database(error.to_string()))?)
    .execute(&mut **transaction)
    .await
    .map_err(|error| map_constraint(error, memory.id, &memory.project, policy))?;
    Ok(())
}

/// Assign an identity to an existing legacy Rule after an exact read token check.
///
/// # Errors
///
/// Returns a typed policy error for an invalid or stale assignment, or a
/// database error if persistence fails.
pub async fn adopt(
    pool: &PgPool,
    id: Uuid,
    policy: &PolicyWrite,
    expected_updated_at: DateTime<Utc>,
) -> Result<MemorySummary, Error> {
    policy
        .validate()
        .map_err(|message| validation(message, id, policy))?;
    let candidate = db::get(pool, id)
        .await
        .map_err(Error::from)?
        .ok_or_else(|| Error::NotFound(format!("memory {id}")))?;
    let project = candidate.project;
    let mut transaction = pool.begin().await.map_err(Error::from)?;
    acquire_identity_lock(&mut transaction, &project, &policy.policy_key).await?;
    adopt_in_transaction(&mut transaction, id, &project, policy, expected_updated_at).await?;
    transaction.commit().await.map_err(Error::from)?;
    load_published(pool, id).await
}

/// Adopt a legacy Rule after its identity lock is held by the transaction.
///
/// # Errors
/// Returns a typed stale or publication error, or a database error.
pub(crate) async fn adopt_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
    project: &str,
    policy: &PolicyWrite,
    expected_updated_at: DateTime<Utc>,
) -> Result<(), Error> {
    policy
        .validate()
        .map_err(|message| validation(message, id, policy))?;
    let head = current_head(transaction, project, &policy.policy_key).await?;
    let mut ids = vec![id];
    if let Some((head_id, _)) = head {
        ids.push(head_id);
    }
    lock_rows(transaction, &ids).await?;
    let row = sqlx::query(
        "SELECT category, project, policy_key, updated_at
         FROM memories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Error::from)?
    .ok_or_else(|| Error::NotFound(format!("memory {id}")))?;
    let category: Category = row.try_get("category").map_err(Error::from)?;
    let locked_project: String = row.try_get("project").map_err(Error::from)?;
    let locked_key: Option<String> = row.try_get("policy_key").map_err(Error::from)?;
    let locked_timestamp: DateTime<Utc> = row.try_get("updated_at").map_err(Error::from)?;
    if category != Category::Rule || locked_project != project {
        return Err(validation(
            "only a legacy Rule may be classified",
            id,
            policy,
        ));
    }
    if locked_key.is_some() {
        return Err(conflict(
            "policy_immutable_revision",
            format!("Rule {id} is already classified; publish a successor"),
            id,
            project,
            policy,
            head.map(|(head_id, _)| head_id),
        ));
    }
    if locked_timestamp != expected_updated_at {
        return Err(Error::Policy {
            code: "policy_stale_assignment".to_owned(),
            message: format!("Rule {id} changed since memory_get; inspect it again"),
            details: serde_json::json!({
                "id": id,
                "project": project,
                "policy_key": policy.policy_key,
                "expected_updated_at": memory_common::policy::format_updated_at(expected_updated_at),
                "actual_updated_at": memory_common::policy::format_updated_at(locked_timestamp),
            }),
            conflict: true,
        });
    }
    validate_publication(transaction, project, policy, head).await?;
    supersede_head(transaction, head).await?;
    sqlx::query(
        "UPDATE memories SET
            policy_key = $2,
            policy_revision = $3,
            policy_delivery_class = $4,
            policy_state = 'active',
            policy_supersedes = $5,
            policy_selectors = $6,
            policy_values = $7,
            updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond')
         WHERE id = $1 AND policy_key IS NULL",
    )
    .bind(id)
    .bind(&policy.policy_key)
    .bind(policy.revision)
    .bind(delivery_class_name(policy.delivery_class))
    .bind(policy.supersedes)
    .bind(
        serde_json::to_value(&policy.selectors)
            .map_err(|error| Error::Database(error.to_string()))?,
    )
    .bind(serde_json::to_value(&policy.values).map_err(|error| Error::Database(error.to_string()))?)
    .execute(&mut **transaction)
    .await
    .map_err(|error| map_constraint(error, id, project, policy))?;
    Ok(())
}

async fn load_published(pool: &PgPool, id: Uuid) -> Result<MemorySummary, Error> {
    db::get(pool, id)
        .await
        .map_err(Error::from)?
        .ok_or_else(|| Error::Database(format!("published policy {id} disappeared")))
}

/// Validate policy metadata before any embedding request is made.
///
/// # Errors
///
/// Returns a policy validation error for malformed metadata or a non-Rule.
pub fn validate_store(policy: &PolicyWrite, category: &Category) -> Result<(), Error> {
    policy
        .validate()
        .map_err(|message| validation(message, Uuid::nil(), policy))?;
    if *category != Category::Rule {
        return Err(validation(
            "policy metadata is allowed only on Rule memories",
            Uuid::nil(),
            policy,
        ));
    }
    Ok(())
}

/// Parse a timezone-qualified exact read token without truncating excess digits.
///
/// # Errors
///
/// Returns a policy validation error for a malformed or zoneless token.
pub fn parse_expected_updated_at(
    value: &str,
    policy: &PolicyWrite,
) -> Result<DateTime<Utc>, Error> {
    let fraction_digits = value.split_once('.').map_or(0, |(_, suffix)| {
        suffix.bytes().take_while(u8::is_ascii_digit).count()
    });
    if fraction_digits > 9 {
        return Err(validation(
            "expected_updated_at may have at most nine fractional digits",
            Uuid::nil(),
            policy,
        ));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| {
            validation(
                "expected_updated_at must be timezone-qualified RFC3339",
                Uuid::nil(),
                policy,
            )
        })
}

fn validation(message: &str, id: Uuid, policy: &PolicyWrite) -> Error {
    Error::Policy {
        code: "policy_invalid_metadata".to_owned(),
        message: message.to_owned(),
        details: serde_json::json!({
            "id": id,
            "policy_key": policy.policy_key,
            "revision": policy.revision,
            "supersedes": policy.supersedes,
        }),
        conflict: false,
    }
}

fn conflict(
    code: &str,
    message: String,
    id: Uuid,
    project: &str,
    policy: &PolicyWrite,
    current_head: Option<Uuid>,
) -> Error {
    Error::Policy {
        code: code.to_owned(),
        message,
        details: serde_json::json!({
            "id": id,
            "project": project,
            "policy_key": policy.policy_key,
            "revision": policy.revision,
            "supersedes": policy.supersedes,
            "current_head": current_head,
        }),
        conflict: true,
    }
}

pub(crate) async fn acquire_identity_lock(
    transaction: &mut Transaction<'_, Postgres>,
    project: &str,
    key: &str,
) -> Result<(), Error> {
    let identity = format!(
        "memory-policy-v1|{}:{project}|{}:{key}",
        project.len(),
        key.len()
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(identity)
        .execute(&mut **transaction)
        .await
        .map_err(Error::from)?;
    Ok(())
}

async fn current_head(
    transaction: &mut Transaction<'_, Postgres>,
    project: &str,
    key: &str,
) -> Result<Option<(Uuid, i64)>, Error> {
    let row = sqlx::query(
        "SELECT id, policy_revision FROM memories
         WHERE project = $1 AND policy_key = $2 AND policy_state = 'active'",
    )
    .bind(project)
    .bind(key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Error::from)?;
    row.map(|row| -> Result<(Uuid, i64), sqlx::Error> {
        Ok((row.try_get("id")?, row.try_get("policy_revision")?))
    })
    .transpose()
    .map_err(Error::from)
}

pub(crate) async fn lock_rows(
    transaction: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<(), Error> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    sqlx::query("SELECT id FROM memories WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&ids)
        .fetch_all(&mut **transaction)
        .await
        .map_err(Error::from)?;
    Ok(())
}

async fn validate_publication(
    transaction: &mut Transaction<'_, Postgres>,
    project: &str,
    policy: &PolicyWrite,
    head: Option<(Uuid, i64)>,
) -> Result<(), Error> {
    let duplicate: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM memories
         WHERE project = $1 AND policy_key = $2 AND policy_revision = $3",
    )
    .bind(project)
    .bind(&policy.policy_key)
    .bind(policy.revision)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Error::from)?;
    if let Some(id) = duplicate {
        return Err(conflict(
            "policy_duplicate_revision",
            format!(
                "revision {} already exists for {project}/{}",
                policy.revision, policy.policy_key
            ),
            id,
            project,
            policy,
            head.map(|(head_id, _)| head_id),
        ));
    }
    match head {
        None if policy.supersedes.is_none() => Ok(()),
        Some((head_id, head_revision))
            if policy.supersedes == Some(head_id) && policy.revision > head_revision =>
        {
            Ok(())
        }
        _ => Err(conflict(
            "policy_stale_head",
            format!(
                "policy {project}/{} requires the current active predecessor and a higher revision",
                policy.policy_key
            ),
            policy.supersedes.unwrap_or_else(Uuid::nil),
            project,
            policy,
            head.map(|(head_id, _)| head_id),
        )),
    }
}

async fn supersede_head(
    transaction: &mut Transaction<'_, Postgres>,
    head: Option<(Uuid, i64)>,
) -> Result<(), Error> {
    if let Some((id, _)) = head {
        sqlx::query(
            "UPDATE memories SET policy_state = 'superseded',
                 updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond')
             WHERE id = $1 AND policy_state = 'active'",
        )
        .bind(id)
        .execute(&mut **transaction)
        .await
        .map_err(Error::from)?;
    }
    Ok(())
}

fn delivery_class_name(value: DeliveryClass) -> &'static str {
    match value {
        DeliveryClass::Contextual => "contextual",
        DeliveryClass::Mandatory => "mandatory",
    }
}

fn map_constraint(error: sqlx::Error, id: Uuid, project: &str, policy: &PolicyWrite) -> Error {
    let code = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint);
    match code {
        Some("memories_policy_revision_unique") => conflict(
            "policy_duplicate_revision",
            format!(
                "revision {} already exists for {project}/{}",
                policy.revision, policy.policy_key
            ),
            id,
            project,
            policy,
            None,
        ),
        Some("memories_policy_one_active") => conflict(
            "policy_duplicate_head",
            format!(
                "an active revision already exists for {project}/{}",
                policy.policy_key
            ),
            id,
            project,
            policy,
            None,
        ),
        Some("memories_policy_predecessor_fk" | "memories_policy_shape") => {
            validation("policy predecessor or metadata is invalid", id, policy)
        }
        _ => Error::from(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use memory_common::policy::{DeliveryClass, PolicyState, PolicyWrite};
    use sqlx::{PgPool, Row};
    use tokio::sync::Barrier;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::db;
    use crate::error::Error;
    use crate::model::{Category, Memory};

    use super::{adopt, publish_new};

    fn memory(project: &str) -> Memory {
        let now = Utc::now();
        Memory {
            id: Uuid::new_v4(),
            policy: None,
            category: Category::Rule,
            content: "Keep an immutable revision.".to_owned(),
            created_at: now,
            embedding: vec![0.0; 1024],
            project: project.to_owned(),
            summary: "Immutable Rule".to_owned(),
            tags: vec!["shared".to_owned()],
            updated_at: now,
        }
    }

    fn write(key: &str, revision: i64, supersedes: Option<Uuid>) -> PolicyWrite {
        PolicyWrite {
            policy_key: key.to_owned(),
            revision,
            delivery_class: DeliveryClass::Contextual,
            supersedes,
            selectors: memory_common::policy::PolicySelectors::default(),
            values: std::collections::BTreeMap::new(),
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn scoped_metadata_round_trips_and_sql_rejects_bad_shapes(pool: PgPool) {
        let source = memory("app");
        let mut policy = write("storage.host", 1, None);
        policy.selectors.profile = Some(BTreeSet::from(["workstation-host".to_owned()]));
        policy
            .values
            .insert("build.target".to_owned(), "persistent-disk".to_owned());
        let published = publish_new(&pool, &source, &policy).await.unwrap();
        let stored = published.policy.unwrap();
        assert_eq!(stored.selectors, policy.selectors);
        assert_eq!(stored.values, policy.values);
        let candidates = db::load_rule_candidates(&pool, "app").await.unwrap();
        assert_eq!(
            candidates[0].policy.as_ref().unwrap().selectors,
            policy.selectors
        );

        for sql in [
            "UPDATE memories SET policy_selectors = '[]'::jsonb WHERE id = $1",
            "UPDATE memories SET policy_selectors = '{\"other\":[]}'::jsonb WHERE id = $1",
            "UPDATE memories SET policy_selectors = '{\"phase\":null}'::jsonb WHERE id = $1",
            "UPDATE memories SET policy_selectors = '{\"phase\":[42]}'::jsonb WHERE id = $1",
            "UPDATE memories SET policy_values = '[]'::jsonb WHERE id = $1",
            "UPDATE memories SET policy_values = '{\"build.target\":42}'::jsonb WHERE id = $1",
        ] {
            assert!(
                sqlx::query(sql)
                    .bind(source.id)
                    .execute(&pool)
                    .await
                    .is_err()
            );
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn scope_downgrade_refuses_loss_and_empty_extensions_roll_back(pool: PgPool) {
        let source = memory("app");
        publish_new(&pool, &source, &write("plain", 1, None))
            .await
            .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20260927000000_policy_scope.down.sql"
        ))
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20260927000000_policy_scope.up.sql"
        ))
        .execute(&mut *transaction)
        .await
        .unwrap();
        transaction.rollback().await.unwrap();
        assert!(db::get(&pool, source.id).await.unwrap().is_some());

        let scoped = memory("app");
        let mut policy = write("scoped", 1, None);
        policy.selectors.language = Some(BTreeSet::from(["rust".to_owned()]));
        publish_new(&pool, &scoped, &policy).await.unwrap();
        let mut transaction = pool.begin().await.unwrap();
        assert!(
            sqlx::raw_sql(include_str!(
                "../../migrations/20260927000000_policy_scope.down.sql"
            ))
            .execute(&mut *transaction)
            .await
            .is_err()
        );
        transaction.rollback().await.unwrap();
        assert_eq!(
            db::get(&pool, scoped.id)
                .await
                .unwrap()
                .unwrap()
                .policy
                .unwrap()
                .selectors,
            policy.selectors
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn roots_successors_and_legacy_adoption_keep_history(pool: PgPool) {
        let root = memory("project-a");
        let first = publish_new(&pool, &root, &write("build.storage", 7, None))
            .await
            .unwrap();
        assert_eq!(first.id, root.id);
        assert_eq!(first.policy.as_ref().unwrap().state, PolicyState::Active);

        let successor = memory("project-a");
        let second = publish_new(&pool, &successor, &write("build.storage", 9, Some(root.id)))
            .await
            .unwrap();
        assert_eq!(second.policy.as_ref().unwrap().supersedes, Some(root.id));
        let historical = db::get(&pool, root.id).await.unwrap().unwrap();
        assert_eq!(historical.policy.unwrap().state, PolicyState::Superseded);
        let rules = db::list_rules(&pool, "project-a", false, false, None)
            .await
            .unwrap();
        assert_eq!(
            rules.iter().map(|row| row.id).collect::<Vec<_>>(),
            [second.id]
        );
        let recall = db::list_core(&pool, "project-a", false).await.unwrap();
        assert!(recall.iter().any(|row| row.id == second.id));
        assert!(!recall.iter().any(|row| row.id == root.id));

        let independent = memory("general");
        publish_new(&pool, &independent, &write("build.storage", 1, None))
            .await
            .unwrap();
        let legacy = memory("project-a");
        db::insert(&pool, &legacy).await.unwrap();
        let legacy_before = db::get(&pool, legacy.id).await.unwrap().unwrap();
        let embedding_before: String =
            sqlx::query_scalar("SELECT embedding::TEXT FROM memories WHERE id = $1")
                .bind(legacy.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let token = legacy_before.updated_at;
        let adopted = adopt(&pool, legacy.id, &write("other.policy", 1, None), token)
            .await
            .unwrap();
        assert_eq!(adopted.id, legacy.id);
        assert_eq!(adopted.content, legacy.content);
        assert_eq!(adopted.summary, legacy_before.summary);
        assert_eq!(adopted.tags, legacy_before.tags);
        assert_eq!(adopted.created_at, legacy_before.created_at);
        assert_eq!(adopted.policy.unwrap().policy_key, "other.policy");
        let embedding_after: String =
            sqlx::query_scalar("SELECT embedding::TEXT FROM memories WHERE id = $1")
                .bind(legacy.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(embedding_after, embedding_before);
        assert!(
            !db::update(&pool, legacy.id, Some("changed"), None, None, None)
                .await
                .unwrap()
        );
        assert!(!db::delete(&pool, legacy.id).await.unwrap());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn stale_assignment_follows_strictly_advancing_legacy_tokens(pool: PgPool) {
        let legacy = memory("project-a");
        db::insert(&pool, &legacy).await.unwrap();
        let old = db::get(&pool, legacy.id).await.unwrap().unwrap().updated_at;
        for index in 0..3 {
            assert!(
                db::update(
                    &pool,
                    legacy.id,
                    None,
                    None,
                    Some(&format!("edit {index}")),
                    None
                )
                .await
                .unwrap()
            );
        }
        let changed = db::get(&pool, legacy.id).await.unwrap().unwrap();
        assert!(changed.updated_at > old);
        let policy = write("build.storage", 1, None);
        let error = adopt(&pool, legacy.id, &policy, old).await.err().unwrap();
        assert!(matches!(error, Error::Policy { code, .. } if code == "policy_stale_assignment"));
        let after = db::get(&pool, legacy.id).await.unwrap().unwrap();
        assert_eq!(after.updated_at, changed.updated_at);
        assert!(after.policy.is_none());
        assert_eq!(after.summary, "edit 2");
        assert!(
            adopt(&pool, legacy.id, &policy, after.updated_at)
                .await
                .is_ok()
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn constraints_and_classified_down_refusal_are_atomic(pool: PgPool) {
        let legacy = memory("project-a");
        db::insert(&pool, &legacy).await.unwrap();
        let partial = sqlx::query("UPDATE memories SET policy_key = 'invalid' WHERE id = $1")
            .bind(legacy.id)
            .execute(&pool)
            .await;
        assert!(partial.is_err());

        let direct_assignment = "UPDATE memories SET policy_key = $2, policy_revision = $3,
             policy_delivery_class = 'contextual', policy_state = 'active',
             policy_supersedes = $4 WHERE id = $1";
        for (key, revision, predecessor) in
            [("Bad", 1, None), ("bad.link", 1, Some(Uuid::new_v4()))]
        {
            assert!(
                sqlx::query(direct_assignment)
                    .bind(legacy.id)
                    .bind(key)
                    .bind(revision)
                    .bind(predecessor)
                    .execute(&pool)
                    .await
                    .is_err()
            );
        }

        publish_new(
            &pool,
            &memory("project-a"),
            &write("build.storage", 1, None),
        )
        .await
        .unwrap();
        for revision in [1_i64, 2] {
            assert!(
                sqlx::query(direct_assignment)
                    .bind(legacy.id)
                    .bind("build.storage")
                    .bind(revision)
                    .bind(None::<Uuid>)
                    .execute(&pool)
                    .await
                    .is_err()
            );
        }
        let down = sqlx::raw_sql(include_str!(
            "../../migrations/20260926000000_policy_identity.down.sql"
        ))
        .execute(&pool)
        .await;
        assert!(down.is_err());
        let columns = sqlx::query(
            "SELECT column_name FROM information_schema.columns
             WHERE table_name = 'memories' AND column_name = 'policy_key'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(columns.len(), 1);
        let migration: bool = sqlx::query_scalar(
            "SELECT success FROM _sqlx_migrations WHERE version = 20260926000000",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(migration);
        let row = sqlx::query("SELECT policy_key FROM memories WHERE id = $1")
            .bind(legacy.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>, _>("policy_key").unwrap(),
            None
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unclassified_down_and_up_preserve_legacy_rows(pool: PgPool) {
        let legacy = memory("project-a");
        db::insert(&pool, &legacy).await.unwrap();
        let before = db::get(&pool, legacy.id).await.unwrap().unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20260926000000_policy_identity.down.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20260926000000_policy_identity.up.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let after = db::get(&pool, legacy.id).await.unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.content, before.content);
        assert_eq!(after.summary, before.summary);
        assert_eq!(after.tags, before.tags);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.updated_at, before.updated_at);
        assert!(after.policy.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn competing_roots_and_successors_leave_one_active_head(pool: PgPool) {
        let mut gate = pool.begin().await.unwrap();
        super::acquire_identity_lock(&mut gate, "project-a", "build.storage")
            .await
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let worker_pool = pool.clone();
            let worker_barrier = barrier.clone();
            workers.push(tokio::spawn(async move {
                worker_barrier.wait().await;
                publish_new(
                    &worker_pool,
                    &memory("project-a"),
                    &write("build.storage", 1, None),
                )
                .await
            }));
        }
        barrier.wait().await;
        gate.commit().await.unwrap();
        let mut results = Vec::new();
        for worker in workers {
            results.push(
                timeout(Duration::from_secs(10), worker)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let head = results.into_iter().find_map(Result::ok).unwrap();

        let mut gate = pool.begin().await.unwrap();
        super::acquire_identity_lock(&mut gate, "project-a", "build.storage")
            .await
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let worker_pool = pool.clone();
            let worker_barrier = barrier.clone();
            let predecessor = head.id;
            workers.push(tokio::spawn(async move {
                worker_barrier.wait().await;
                publish_new(
                    &worker_pool,
                    &memory("project-a"),
                    &write("build.storage", 2, Some(predecessor)),
                )
                .await
            }));
        }
        barrier.wait().await;
        gate.commit().await.unwrap();
        let mut results = Vec::new();
        for worker in workers {
            results.push(
                timeout(Duration::from_secs(10), worker)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memories
             WHERE project = 'project-a' AND policy_key = 'build.storage'
               AND policy_state = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active, 1);
        assert_eq!(
            db::get(&pool, head.id)
                .await
                .unwrap()
                .unwrap()
                .policy
                .unwrap()
                .state,
            PolicyState::Superseded
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn failed_successor_insert_rolls_back_head_transition(pool: PgPool) {
        let root = memory("project-a");
        publish_new(&pool, &root, &write("build.storage", 1, None))
            .await
            .unwrap();
        let collision = memory("project-a");
        db::insert(&pool, &collision).await.unwrap();
        let mut successor = memory("project-a");
        successor.id = collision.id;
        assert!(
            publish_new(&pool, &successor, &write("build.storage", 2, Some(root.id)))
                .await
                .is_err()
        );
        let head = db::get(&pool, root.id).await.unwrap().unwrap();
        assert_eq!(head.policy.unwrap().state, PolicyState::Active);
        assert!(
            db::get(&pool, collision.id)
                .await
                .unwrap()
                .unwrap()
                .policy
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn concurrent_adoptions_of_one_uuid_choose_one_identity(pool: PgPool) {
        let candidate = memory("project-a");
        db::insert(&pool, &candidate).await.unwrap();
        let token = db::get(&pool, candidate.id)
            .await
            .unwrap()
            .unwrap()
            .updated_at;
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for key in ["first.policy", "second.policy"] {
            let worker_pool = pool.clone();
            let worker_barrier = barrier.clone();
            let id = candidate.id;
            workers.push(tokio::spawn(async move {
                worker_barrier.wait().await;
                adopt(&worker_pool, id, &write(key, 1, None), token).await
            }));
        }
        barrier.wait().await;
        let mut results = Vec::new();
        for worker in workers {
            results.push(
                timeout(Duration::from_secs(10), worker)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let row = db::get(&pool, candidate.id).await.unwrap().unwrap();
        let policy = row.policy.unwrap();
        assert!(matches!(
            policy.policy_key.as_str(),
            "first.policy" | "second.policy"
        ));
        assert_eq!(policy.state, PolicyState::Active);
        let identified: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memories WHERE id = $1 AND policy_key IS NOT NULL",
        )
        .bind(candidate.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(identified, 1);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn adoption_racing_legacy_mutations_keeps_one_outcome(pool: PgPool) {
        for change in ["content", "summary", "tags", "delete"] {
            let candidate = memory("project-a");
            db::insert(&pool, &candidate).await.unwrap();
            let token = db::get(&pool, candidate.id)
                .await
                .unwrap()
                .unwrap()
                .updated_at;
            let barrier = Arc::new(Barrier::new(3));
            let adoption_pool = pool.clone();
            let adoption_barrier = barrier.clone();
            let id = candidate.id;
            let adoption = tokio::spawn(async move {
                adoption_barrier.wait().await;
                adopt(
                    &adoption_pool,
                    id,
                    &write(&format!("race.{change}"), 1, None),
                    token,
                )
                .await
            });
            let mutation_pool = pool.clone();
            let mutation_barrier = barrier.clone();
            let mutation = tokio::spawn(async move {
                mutation_barrier.wait().await;
                if change == "delete" {
                    db::delete(&mutation_pool, id).await
                } else {
                    let tags = ["changed".to_owned()];
                    db::update(
                        &mutation_pool,
                        id,
                        (change == "content").then_some("changed"),
                        None,
                        (change == "summary").then_some("changed"),
                        (change == "tags").then_some(tags.as_slice()),
                    )
                    .await
                }
            });
            barrier.wait().await;
            let (adoption, mutation) = timeout(Duration::from_secs(10), async {
                tokio::join!(adoption, mutation)
            })
            .await
            .unwrap();
            let adoption = adoption.unwrap();
            let mutated = mutation.unwrap().unwrap();
            match adoption {
                Ok(classified) => {
                    assert!(!mutated, "{change} overwrote a classified revision");
                    assert!(classified.policy.is_some());
                }
                Err(error) => {
                    assert!(mutated, "{change} should win when adoption fails");
                    assert!(matches!(error, Error::Policy { .. } | Error::NotFound(_)));
                    if change != "delete" {
                        let current = db::get(&pool, id).await.unwrap().unwrap();
                        assert!(current.policy.is_none());
                        assert!(current.updated_at > token);
                    }
                }
            }
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rule_order_keeps_distinct_keys_and_legacy_rows(pool: PgPool) {
        for (key, id) in [
            ("z.key", Uuid::from_u128(10)),
            ("a.key", Uuid::from_u128(11)),
            ("m.key", Uuid::from_u128(12)),
        ] {
            let mut row = memory("project-a");
            row.id = id;
            publish_new(&pool, &row, &write(key, 1, None))
                .await
                .unwrap();
        }
        let mut general = memory("general");
        general.id = Uuid::from_u128(13);
        publish_new(&pool, &general, &write("a.key", 1, None))
            .await
            .unwrap();
        for (id, updated_at) in [
            (Uuid::from_u128(14), "2098-09-26T12:34:27Z"),
            (Uuid::from_u128(15), "2099-09-26T12:34:27Z"),
        ] {
            let mut row = memory("project-a");
            row.id = id;
            db::insert(&pool, &row).await.unwrap();
            sqlx::query("UPDATE memories SET updated_at = $2 WHERE id = $1")
                .bind(id)
                .bind(chrono::DateTime::parse_from_rfc3339(updated_at).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        for shadow_general in [false, true] {
            let rules = db::list_rules(
                &pool,
                "project-a",
                true,
                shadow_general,
                Some(&["shared".to_owned()]),
            )
            .await
            .unwrap();
            assert_eq!(
                rules.iter().map(|row| row.id).collect::<Vec<_>>(),
                [13, 11, 12, 10, 15, 14].map(Uuid::from_u128)
            );
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rule_read_sees_old_or_new_head_at_publication_commit(pool: PgPool) {
        let root = memory("project-a");
        publish_new(&pool, &root, &write("build.storage", 1, None))
            .await
            .unwrap();
        let successor_id = Uuid::new_v4();
        let mut transaction = pool.begin().await.unwrap();
        super::acquire_identity_lock(&mut transaction, "project-a", "build.storage")
            .await
            .unwrap();
        sqlx::query("UPDATE memories SET policy_state = 'superseded' WHERE id = $1")
            .bind(root.id)
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO memories
                (id, category, content, created_at, embedding, project, summary, tags,
                 updated_at, policy_key, policy_revision, policy_delivery_class,
                 policy_state, policy_supersedes)
             SELECT $2, category, content, created_at, embedding, project, summary, tags,
                    clock_timestamp(), policy_key, 2, policy_delivery_class,
                    'active', id
             FROM memories WHERE id = $1",
        )
        .bind(root.id)
        .bind(successor_id)
        .execute(&mut *transaction)
        .await
        .unwrap();
        let before = db::list_rules(&pool, "project-a", false, false, None)
            .await
            .unwrap();
        assert_eq!(
            before.iter().map(|row| row.id).collect::<Vec<_>>(),
            [root.id]
        );
        transaction.commit().await.unwrap();
        let after = db::list_rules(&pool, "project-a", false, false, None)
            .await
            .unwrap();
        assert_eq!(
            after.iter().map(|row| row.id).collect::<Vec<_>>(),
            [successor_id]
        );
    }
}
