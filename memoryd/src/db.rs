use std::collections::HashSet;

use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::model::{
    Category, Memory, MemoryEdge, MemoryEdgeSummary, MemorySummary, Session, SessionLog,
    SessionLogChunk, SessionLogSummary, SessionMessage, SessionMessageSummary, SessionSummary,
};
use crate::workflow;

const WORKFLOW_PROVENANCE_MUTEX: i64 = 0x4d45_4d57_4650_524f;

/// Load all embeddings for one project.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn all_embeddings(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<(Uuid, Vec<f32>)>, sqlx::Error> {
    let rows =
        sqlx::query("SELECT id, embedding::TEXT FROM memories WHERE project = $1 ORDER BY id")
            .bind(project)
            .fetch_all(pool)
            .await?;
    rows.iter()
        .map(|row| {
            let id: Uuid = row.try_get("id")?;
            let embedding_text: String = row.try_get("embedding")?;
            let embedding = parse_pgvector_text(&embedding_text);
            Ok((id, embedding))
        })
        .collect()
}

/// Load embeddings eligible for destructive dream maintenance.
///
/// Public graph refresh intentionally continues to use [`all_embeddings`].
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn maintenance_embeddings(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<(Uuid, Vec<f32>)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, embedding::TEXT
         FROM memories
         WHERE project = $1
           AND workflow_artifact = FALSE
           AND category NOT IN ('plan', 'rule')
         ORDER BY id",
    )
    .bind(project)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let id: Uuid = row.try_get("id")?;
            let embedding_text: String = row.try_get("embedding")?;
            Ok((id, parse_pgvector_text(&embedding_text)))
        })
        .collect()
}

/// Connect to `PostgreSQL`.
///
/// # Errors
///
/// Returns an error if the pool cannot be created or the database cannot be reached.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await
}

/// Create or upsert a normalized session row.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn create_session(
    pool: &PgPool,
    session: &Session,
) -> Result<SessionSummary, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    acquire_external_session_mutex(&mut transaction, &session.external_session_id).await?;
    let row = sqlx::query(
        "INSERT INTO sessions
            (id, created_at, updated_at, ended_at, cwd, project,
             external_session_id, agent, workflow_artifact)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (external_session_id) DO UPDATE SET
            updated_at = EXCLUDED.updated_at,
            cwd = EXCLUDED.cwd,
            project = EXCLUDED.project,
            workflow_artifact = sessions.workflow_artifact
                OR EXCLUDED.workflow_artifact,
            agent = CASE
                WHEN EXCLUDED.agent = '' THEN sessions.agent
                ELSE EXCLUDED.agent
            END
         RETURNING id, created_at, updated_at, ended_at, cwd, project,
                   external_session_id, agent, workflow_artifact",
    )
    .bind(session.id)
    .bind(session.created_at)
    .bind(session.updated_at)
    .bind(session.ended_at)
    .bind(&session.cwd)
    .bind(&session.project)
    .bind(&session.external_session_id)
    .bind(&session.agent)
    .bind(session.workflow_artifact)
    .fetch_one(&mut *transaction)
    .await?;
    let id: Uuid = row.try_get("id")?;
    let mut resolved: bool = row.try_get("workflow_artifact")?;
    let log = sqlx::query(
        "SELECT id, workflow_artifact
         FROM session_logs
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(&session.external_session_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(log) = log {
        let log_id: Uuid = log.try_get("id")?;
        resolved |= log.try_get::<bool, _>("workflow_artifact")?;
        reconcile_session_parents(&mut transaction, id, log_id, resolved).await?;
    }
    let summary = row_to_session_summary(&row)?;
    transaction.commit().await?;
    Ok(summary)
}

/// Delete one memory by ID.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Delete all edges involving a specific memory.
///
/// This is a safety net; `ON DELETE CASCADE` should handle this automatically.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn delete_edges_for_memory(pool: &PgPool, memory_id: Uuid) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM memory_edges WHERE src_id = $1 OR dst_id = $1")
        .bind(memory_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Delete edges originating from a memory with specific origins.
///
/// Used to clear stale write-time edges before rebuilding.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn delete_edges_by_origins(
    pool: &PgPool,
    src_id: Uuid,
    origins: &[crate::model::EdgeOrigin],
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM memory_edges WHERE src_id = $1 AND origin = ANY($2)")
        .bind(src_id)
        .bind(origins)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// List active neighbor memories reachable from a given memory via edges.
///
/// Returns the neighbor memory summaries along with the edge weight.
/// Only follows non-suppressed edges.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_neighbors(
    pool: &PgPool,
    memory_id: Uuid,
    limit: i64,
) -> Result<Vec<(MemoryEdgeSummary, MemorySummary)>, sqlx::Error> {
    let rows = sqlx::query(
        "WITH ranked AS (
            SELECT e.id AS edge_id, e.confidence, e.created_at AS edge_created_at,
                   e.dst_id, e.dst_project, e.evidence, e.origin, e.relation,
                   e.src_id, e.src_project, e.suppressed, e.updated_at AS edge_updated_at,
                   e.weight,
                   CASE WHEN e.src_id = $1 THEN e.dst_id ELSE e.src_id END AS neighbor_id,
                   ROW_NUMBER() OVER (
                       PARTITION BY CASE WHEN e.src_id = $1 THEN e.dst_id ELSE e.src_id END,
                                    e.relation, e.origin
                       ORDER BY e.weight DESC
                   ) AS rn
            FROM memory_edges e
            WHERE (e.src_id = $1 OR e.dst_id = $1)
              AND NOT e.suppressed
        )
        SELECT r.edge_id, r.confidence, r.edge_created_at, r.dst_id, r.dst_project,
               r.evidence, r.origin, r.relation, r.src_id, r.src_project, r.suppressed,
               r.edge_updated_at, r.weight,
               m.id, m.category, m.content, m.created_at, m.project, m.summary, m.tags, m.updated_at
         FROM ranked r
         JOIN memories m ON m.id = r.neighbor_id
         WHERE r.rn = 1
         ORDER BY r.weight DESC
         LIMIT $2",
    )
    .bind(memory_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let edge = row_to_edge_summary(row)?;
            let memory = row_to_summary(row)?;
            Ok((edge, memory))
        })
        .collect()
}

/// List graph neighbors for semantic search under its artifact policy.
///
/// Public [`list_neighbors`] remains inclusive for audit access.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_search_neighbors(
    pool: &PgPool,
    memory_id: Uuid,
    limit: i64,
    include_workflow_artifacts: bool,
) -> Result<Vec<(MemoryEdgeSummary, MemorySummary)>, sqlx::Error> {
    if include_workflow_artifacts {
        return list_neighbors(pool, memory_id, limit).await;
    }
    let rows = sqlx::query(
        "WITH ranked AS (
            SELECT e.id AS edge_id, e.confidence,
                   e.created_at AS edge_created_at, e.dst_id, e.dst_project,
                   e.evidence, e.origin, e.relation, e.src_id, e.src_project,
                   e.suppressed, e.updated_at AS edge_updated_at, e.weight,
                   CASE WHEN e.src_id = $1 THEN e.dst_id ELSE e.src_id END AS neighbor_id,
                   ROW_NUMBER() OVER (
                       PARTITION BY CASE WHEN e.src_id = $1 THEN e.dst_id ELSE e.src_id END,
                                    e.relation, e.origin
                       ORDER BY e.weight DESC
                   ) AS rn
            FROM memory_edges e
            WHERE (e.src_id = $1 OR e.dst_id = $1)
              AND NOT e.suppressed
        )
        SELECT r.edge_id, r.confidence, r.edge_created_at, r.dst_id,
               r.dst_project, r.evidence, r.origin, r.relation, r.src_id,
               r.src_project, r.suppressed, r.edge_updated_at, r.weight,
               m.id, m.category, m.content, m.created_at, m.project,
               m.summary, m.tags, m.updated_at
        FROM ranked r
        JOIN memories m ON m.id = r.neighbor_id
        WHERE r.rn = 1
          AND m.workflow_artifact = FALSE
        ORDER BY r.weight DESC
        LIMIT $2",
    )
    .bind(memory_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row_to_edge_summary(row)?, row_to_summary(row)?)))
        .collect()
}

/// List edges originating from a specific memory.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_edges_from(
    pool: &PgPool,
    src_id: Uuid,
    limit: i64,
) -> Result<Vec<MemoryEdgeSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id AS edge_id, confidence, created_at AS edge_created_at,
                dst_id, dst_project, evidence, origin, relation,
                src_id, src_project, suppressed, updated_at AS edge_updated_at,
                weight
         FROM memory_edges
         WHERE src_id = $1
         ORDER BY weight DESC
         LIMIT $2",
    )
    .bind(src_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_edge_summary).collect()
}

/// List edges pointing to a specific memory.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_edges_to(
    pool: &PgPool,
    dst_id: Uuid,
    limit: i64,
) -> Result<Vec<MemoryEdgeSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id AS edge_id, confidence, created_at AS edge_created_at,
                dst_id, dst_project, evidence, origin, relation,
                src_id, src_project, suppressed, updated_at AS edge_updated_at,
                weight
         FROM memory_edges
         WHERE dst_id = $1
         ORDER BY weight DESC
         LIMIT $2",
    )
    .bind(dst_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_edge_summary).collect()
}

/// Reinforce an existing edge by increasing its weight.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn reinforce_edge(pool: &PgPool, edge_id: Uuid, boost: f64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE memory_edges SET
            weight = weight + $2,
            updated_at = NOW()
         WHERE id = $1",
    )
    .bind(edge_id)
    .bind(boost)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Suppress maintenance-generated edges that were not refreshed.
///
/// Targets all `embedding_neighbor` and `shared_tag` origin edges
/// where `updated_at` is older than the current cycle timestamp.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn suppress_stale_maintenance_edges(
    pool: &PgPool,
    cycle_start: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE memory_edges SET
            suppressed = TRUE,
            updated_at = NOW()
         WHERE NOT suppressed
           AND origin IN ('embedding_neighbor', 'shared_tag')
           AND updated_at < $1",
    )
    .bind(cycle_start)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Suppress an edge (soft-delete, excluded from traversal).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn suppress_edge(pool: &PgPool, edge_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE memory_edges SET
            suppressed = TRUE,
            updated_at = NOW()
         WHERE id = $1",
    )
    .bind(edge_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Upsert an edge between two memories.
///
/// Uses `(src_id, dst_id, relation, origin)` as the idempotent key.
/// On conflict, updates weight, confidence, evidence, and timestamps.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn upsert_edge(pool: &PgPool, edge: &MemoryEdge) -> Result<Uuid, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO memory_edges
            (id, src_id, dst_id, src_project, dst_project, relation, origin,
             weight, confidence, evidence, suppressed, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (src_id, dst_id, relation, origin) DO UPDATE SET
            weight = EXCLUDED.weight,
            confidence = EXCLUDED.confidence,
            evidence = EXCLUDED.evidence,
            suppressed = EXCLUDED.suppressed,
            updated_at = EXCLUDED.updated_at
         RETURNING id",
    )
    .bind(edge.id)
    .bind(edge.src_id)
    .bind(edge.dst_id)
    .bind(&edge.src_project)
    .bind(&edge.dst_project)
    .bind(&edge.relation)
    .bind(&edge.origin)
    .bind(edge.weight)
    .bind(edge.confidence)
    .bind(&edge.evidence)
    .bind(edge.suppressed)
    .bind(edge.created_at)
    .bind(edge.updated_at)
    .fetch_one(pool)
    .await?;
    row.try_get("id")
}

/// Append one message to a normalized session.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn append_session_message(
    pool: &PgPool,
    message: &SessionMessage,
) -> Result<Option<SessionMessageSummary>, sqlx::Error> {
    let external_session_id: Option<String> = sqlx::query_scalar(
        "SELECT external_session_id
         FROM sessions
         WHERE id = $1",
    )
    .bind(message.session_id)
    .fetch_optional(pool)
    .await?;
    let Some(external_session_id) = external_session_id else {
        return Ok(None);
    };
    let mut transaction = pool.begin().await?;
    acquire_external_session_mutex(&mut transaction, &external_session_id).await?;
    let session = sqlx::query(
        "SELECT id, workflow_artifact
         FROM sessions
         WHERE id = $1 AND external_session_id = $2
         FOR UPDATE",
    )
    .bind(message.session_id)
    .bind(&external_session_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(session) = session else {
        transaction.commit().await?;
        return Ok(None);
    };
    let incoming_workflow = workflow::contains_task_token(&message.content)
        || message
            .metadata
            .as_deref()
            .is_some_and(workflow::contains_task_token);
    let mut resolved = session.try_get::<bool, _>("workflow_artifact")? || incoming_workflow;
    let row = sqlx::query(
        "INSERT INTO session_messages (id, session_id, created_at, agent, role, kind, content, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, session_id, created_at, agent, role, kind, content, metadata",
    )
    .bind(message.id)
    .bind(message.session_id)
    .bind(message.created_at)
    .bind(&message.agent)
    .bind(&message.role)
    .bind(&message.kind)
    .bind(&message.content)
    .bind(&message.metadata)
    .fetch_one(&mut *transaction)
    .await?;

    sqlx::query(
        "UPDATE sessions
         SET updated_at = GREATEST(updated_at, $2, NOW())
         WHERE id = $1",
    )
    .bind(message.session_id)
    .bind(message.created_at)
    .execute(&mut *transaction)
    .await?;
    let log = sqlx::query(
        "SELECT id, workflow_artifact
         FROM session_logs
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(&external_session_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(log) = log {
        let log_id: Uuid = log.try_get("id")?;
        resolved |= log.try_get::<bool, _>("workflow_artifact")?;
        reconcile_session_parents(&mut transaction, message.session_id, log_id, resolved).await?;
    } else if resolved {
        sqlx::query(
            "UPDATE sessions
             SET workflow_artifact = TRUE,
                 updated_at = NOW()
             WHERE id = $1 AND workflow_artifact = FALSE",
        )
        .bind(message.session_id)
        .execute(&mut *transaction)
        .await?;
    }

    let summary = row_to_session_message_summary(&row)?;
    transaction.commit().await?;
    Ok(Some(summary))
}

/// Fetch one memory by ID.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<MemorySummary>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, category, content, created_at, project, summary, tags, updated_at
         FROM memories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_summary).transpose()
}

/// Fetch one normalized session by ID.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn get_session(pool: &PgPool, id: Uuid) -> Result<Option<SessionSummary>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, created_at, updated_at, ended_at, cwd, project, external_session_id, agent
         FROM sessions
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_session_summary).transpose()
}

/// Fetch one finalized session log by ID.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn get_session_log(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<SessionLogSummary>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, content, created_at, cwd, project, session_id, summary
         FROM session_logs
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_session_log_summary).transpose()
}

/// Insert a new memory row.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn insert(pool: &PgPool, memory: &Memory) -> Result<(), sqlx::Error> {
    let embedding = Vector::from(memory.embedding.clone());
    sqlx::query(
        "INSERT INTO memories (id, category, content, created_at, embedding, project, summary, tags, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(memory.id)
    .bind(&memory.category)
    .bind(&memory.content)
    .bind(memory.created_at)
    .bind(embedding)
    .bind(&memory.project)
    .bind(&memory.summary)
    .bind(&memory.tags)
    .bind(memory.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a Plan or Decision while serializing provenance relationships.
///
/// # Errors
///
/// Returns an error if the transaction fails.
pub async fn insert_with_workflow_provenance(
    pool: &PgPool,
    memory: &Memory,
) -> Result<(), sqlx::Error> {
    let review_targets = workflow::reviewed_item_ids(&memory.tags);
    let mut transaction = pool.begin().await?;
    acquire_workflow_provenance_mutex(&mut transaction).await?;
    let embedding = Vector::from(memory.embedding.clone());
    let directly_scoped =
        memory.category == Category::Plan && workflow::plan_is_directly_scoped(&memory.tags);
    sqlx::query(
        "INSERT INTO memories
            (id, category, content, created_at, embedding, project, summary, tags,
             updated_at, workflow_artifact)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(memory.id)
    .bind(&memory.category)
    .bind(&memory.content)
    .bind(memory.created_at)
    .bind(embedding)
    .bind(&memory.project)
    .bind(&memory.summary)
    .bind(&memory.tags)
    .bind(memory.updated_at)
    .bind(directly_scoped)
    .execute(&mut *transaction)
    .await?;
    promote_workflow_relationships(
        &mut transaction,
        memory.id,
        &memory.category,
        &review_targets,
    )
    .await?;
    transaction.commit().await
}

/// List core memories for a project.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_core(pool: &PgPool, project: &str) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let categories: Vec<Category> = [
        Category::Context,
        Category::Decision,
        Category::ErrorFix,
        Category::Plan,
        Category::Rule,
    ]
    .into_iter()
    .filter(Category::is_core)
    .collect();

    let rows = sqlx::query(
        "SELECT id, category, content, created_at, project, summary, tags, updated_at
         FROM memories
         WHERE project = $1 AND category = ANY($2)
         ORDER BY updated_at DESC",
    )
    .bind(project)
    .bind(&categories)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_summary).collect()
}

/// List memories eligible for destructive dream maintenance.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_maintenance_candidates(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, category, content, created_at, project, summary, tags,
                updated_at
         FROM memories
         WHERE project = $1
           AND workflow_artifact = FALSE
           AND category NOT IN ('plan', 'rule')
         ORDER BY updated_at DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_summary).collect()
}

/// List memories awaiting review, optionally filtered by category.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_review_queue(
    pool: &PgPool,
    project: &str,
    category: Option<&Category>,
    limit: i64,
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1
                   AND category = $2
                   AND tags @> ARRAY['review-needed']::TEXT[]
                 ORDER BY updated_at DESC
                 LIMIT $3",
            )
            .bind(project)
            .bind(cat)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1
                   AND tags @> ARRAY['review-needed']::TEXT[]
                 ORDER BY updated_at DESC
                 LIMIT $2",
            )
            .bind(project)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    rows.iter().map(row_to_summary).collect()
}

/// List durable rules for a project.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_rules(
    pool: &PgPool,
    project: &str,
    include_general: bool,
    shadow_general: bool,
    tags: Option<&[String]>,
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let tag_arr = tags.map(<[String]>::to_vec);
    let apply_shadow = shadow_general && tags.is_some_and(|t| !t.is_empty());
    let rows = if include_general && project != crate::app::GENERAL_RULE_PROJECT {
        sqlx::query(
            "SELECT id, category, content, created_at, project, summary, tags, updated_at
             FROM memories
             WHERE category = $1
               AND (
                   project = $2
                   OR (project = $3 AND NOT ($5 AND $4::TEXT[] IS NOT NULL AND EXISTS (
                       SELECT 1 FROM memories m2
                       WHERE m2.category = $1
                         AND m2.project = $2
                         AND m2.tags @> $4::TEXT[]
                   )))
               )
               AND ($4::TEXT[] IS NULL OR tags @> $4::TEXT[])
             ORDER BY CASE WHEN project = $3 THEN 0 ELSE 1 END, updated_at DESC",
        )
        .bind(Category::Rule)
        .bind(project)
        .bind(crate::app::GENERAL_RULE_PROJECT)
        .bind(&tag_arr)
        .bind(apply_shadow)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT id, category, content, created_at, project, summary, tags, updated_at
             FROM memories
             WHERE category = $1 AND project = $2
               AND ($3::TEXT[] IS NULL OR tags @> $3::TEXT[])
             ORDER BY updated_at DESC",
        )
        .bind(Category::Rule)
        .bind(project)
        .bind(&tag_arr)
        .fetch_all(pool)
        .await?
    };
    rows.iter().map(row_to_summary).collect()
}

/// List memories for a project with optional filtering.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list(
    pool: &PgPool,
    project: &str,
    category: Option<&Category>,
    limit: i64,
    offset: i64,
    tags: Option<&[String]>,
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let tag_arr = tags.map(<[String]>::to_vec);
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1 AND category = $2
                   AND ($5::TEXT[] IS NULL OR tags @> $5::TEXT[])
                 ORDER BY updated_at DESC
                 LIMIT $3 OFFSET $4",
            )
            .bind(project)
            .bind(cat)
            .bind(limit)
            .bind(offset)
            .bind(&tag_arr)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1
                   AND ($4::TEXT[] IS NULL OR tags @> $4::TEXT[])
                 ORDER BY updated_at DESC
                 LIMIT $2 OFFSET $3",
            )
            .bind(project)
            .bind(limit)
            .bind(offset)
            .bind(&tag_arr)
            .fetch_all(pool)
            .await?
        }
    };
    rows.iter().map(row_to_summary).collect()
}

/// List all known projects.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_projects(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT project
         FROM (
             SELECT project FROM memories
             UNION
             SELECT project FROM session_logs
             UNION
             SELECT project FROM sessions
         ) AS projects
         WHERE project <> ''
         ORDER BY project",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(|row| row.try_get("project")).collect()
}

/// List finalized session logs for a project.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_session_logs(
    pool: &PgPool,
    project: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SessionLogSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, content, created_at, cwd, project, session_id, summary
         FROM session_logs
         WHERE project = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(project)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_session_log_summary).collect()
}

/// List normalized sessions for a project.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_sessions(
    pool: &PgPool,
    project: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SessionSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, created_at, updated_at, ended_at, cwd, project, external_session_id, agent
         FROM sessions
         WHERE project = $1
         ORDER BY updated_at DESC, id DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(project)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_session_summary).collect()
}

/// Run all SQL migrations.
///
/// # Errors
///
/// Returns an error if migrations cannot be applied.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../migrations").run(pool).await
}

/// List messages for one normalized session.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_session_messages(
    pool: &PgPool,
    session_id: Uuid,
) -> Result<Vec<SessionMessageSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, session_id, created_at, agent, role, kind, content, metadata
         FROM session_messages
         WHERE session_id = $1
         ORDER BY created_at ASC, id ASC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_session_message_summary).collect()
}

const SESSION_SEARCH_INCLUSIVE_SQL: &str = r"
    WITH nearest_chunks AS (
        SELECT c.session_log_id, c.embedding <=> $1 AS distance
        FROM session_log_chunks c
        JOIN session_logs sl ON sl.id = c.session_log_id
        WHERE sl.project = $2
        ORDER BY c.embedding <=> $1
        LIMIT $3
    ), vector_results AS (
        SELECT session_log_id AS id,
               ROW_NUMBER() OVER (ORDER BY MIN(distance)) AS rank_v
        FROM nearest_chunks
        WHERE 1 - distance >= $4
        GROUP BY session_log_id
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        ) AS rank_f
        FROM session_logs
        WHERE project = $2
          AND fts @@ plainto_tsquery('english', $5)
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        LIMIT $3
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT s.id, s.content, s.created_at, s.cwd, s.project, s.session_id,
           s.summary, c.rrf_score AS similarity
    FROM combined c
    JOIN session_logs s ON s.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $6";

const SESSION_SEARCH_DEFAULT_SQL: &str = r"
    WITH nearest_chunks AS (
        SELECT c.session_log_id, c.embedding <=> $1 AS distance
        FROM session_log_chunks c
        JOIN session_logs sl ON sl.id = c.session_log_id
        WHERE sl.project = $2
          AND c.workflow_artifact = FALSE
          AND sl.workflow_artifact = FALSE
        ORDER BY c.embedding <=> $1
        LIMIT $3
    ), vector_results AS (
        SELECT session_log_id AS id,
               ROW_NUMBER() OVER (ORDER BY MIN(distance)) AS rank_v
        FROM nearest_chunks
        WHERE 1 - distance >= $4
        GROUP BY session_log_id
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        ) AS rank_f
        FROM session_logs
        WHERE project = $2
          AND workflow_artifact = FALSE
          AND fts @@ plainto_tsquery('english', $5)
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        LIMIT $3
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT s.id, s.content, s.created_at, s.cwd, s.project, s.session_id,
           s.summary, c.rrf_score AS similarity
    FROM combined c
    JOIN session_logs s ON s.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $6";

/// Search finalized session logs.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn session_log_search(
    pool: &PgPool,
    embedding: Vec<f32>,
    query: &str,
    project: &str,
    limit: i64,
    min_similarity: f64,
    include_workflow_artifacts: bool,
) -> Result<Vec<(SessionLogSummary, f64)>, sqlx::Error> {
    let query_vec = Vector::from(embedding);
    let fetch_limit = limit * 3;
    let statement = if include_workflow_artifacts {
        SESSION_SEARCH_INCLUSIVE_SQL
    } else {
        SESSION_SEARCH_DEFAULT_SQL
    };
    let rows = sqlx::query(statement)
        .bind(&query_vec)
        .bind(project)
        .bind(fetch_limit)
        .bind(min_similarity)
        .bind(query)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| {
            let log = row_to_session_log_summary(row)?;
            let similarity: f64 = row.try_get("similarity")?;
            Ok((log, similarity))
        })
        .collect()
}

/// Atomically publish a session log and all of its chunks.
///
/// When `normalized_session_id` is present, the correlated normalized session
/// must still exist after the external-session mutex is acquired. The returned
/// `None` preserves finalization's not-found result when it disappeared.
///
/// # Errors
///
/// Returns an error if locking, reconciliation, or publication fails.
pub async fn publish_session_log(
    pool: &PgPool,
    log: &SessionLog,
    chunks: &[SessionLogChunk],
    normalized_session_id: Option<Uuid>,
    finalized_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Option<Uuid>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    acquire_external_session_mutex(&mut transaction, &log.session_id).await?;
    let session = sqlx::query(
        "SELECT id, workflow_artifact
         FROM sessions
         WHERE external_session_id = $1
         FOR UPDATE",
    )
    .bind(&log.session_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(expected_id) = normalized_session_id
        && session
            .as_ref()
            .and_then(|row| row.try_get::<Uuid, _>("id").ok())
            != Some(expected_id)
    {
        transaction.commit().await?;
        return Ok(None);
    }
    let existing_log = sqlx::query(
        "SELECT id, workflow_artifact
         FROM session_logs
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(&log.session_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let session_workflow = match &session {
        Some(row) => row.try_get("workflow_artifact")?,
        None => false,
    };
    let existing_log_workflow = match &existing_log {
        Some(row) => row.try_get("workflow_artifact")?,
        None => false,
    };
    let resolved = log.workflow_artifact || session_workflow || existing_log_workflow;
    let embedding = Vector::from(log.embedding.clone());
    let stored_id: Uuid = sqlx::query_scalar(
        "INSERT INTO session_logs
            (id, content, created_at, cwd, embedding, project, session_id,
             summary, workflow_artifact)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (session_id) DO UPDATE SET
            content = EXCLUDED.content,
            cwd = EXCLUDED.cwd,
            embedding = EXCLUDED.embedding,
            project = EXCLUDED.project,
            summary = EXCLUDED.summary,
            workflow_artifact = session_logs.workflow_artifact
                OR EXCLUDED.workflow_artifact
         RETURNING id",
    )
    .bind(log.id)
    .bind(&log.content)
    .bind(log.created_at)
    .bind(&log.cwd)
    .bind(embedding)
    .bind(&log.project)
    .bind(&log.session_id)
    .bind(&log.summary)
    .bind(resolved)
    .fetch_one(&mut *transaction)
    .await?;

    if let Some(session) = &session {
        let session_id: Uuid = session.try_get("id")?;
        sqlx::query(
            "UPDATE sessions
             SET workflow_artifact = workflow_artifact OR $2,
                 updated_at = CASE
                     WHEN $3::TIMESTAMPTZ IS NULL THEN NOW()
                     ELSE GREATEST(updated_at, $3, NOW())
                 END,
                 ended_at = CASE
                     WHEN $3::TIMESTAMPTZ IS NULL THEN ended_at
                     ELSE COALESCE(ended_at, $3)
                 END
             WHERE id = $1",
        )
        .bind(session_id)
        .bind(resolved)
        .bind(finalized_at)
        .execute(&mut *transaction)
        .await?;
    }

    replace_published_chunks(&mut transaction, stored_id, chunks, resolved).await?;
    transaction.commit().await?;
    Ok(Some(stored_id))
}

async fn replace_published_chunks(
    transaction: &mut Transaction<'_, Postgres>,
    stored_id: Uuid,
    chunks: &[SessionLogChunk],
    workflow_artifact: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM session_log_chunks WHERE session_log_id = $1")
        .bind(stored_id)
        .execute(&mut **transaction)
        .await?;
    for chunk in chunks {
        let embedding = Vector::from(chunk.embedding.clone());
        sqlx::query(
            "INSERT INTO session_log_chunks
                (id, chunk_index, content, embedding, session_log_id,
                 workflow_artifact)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(chunk.id)
        .bind(chunk.chunk_index)
        .bind(&chunk.content)
        .bind(embedding)
        .bind(stored_id)
        .bind(workflow_artifact)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

/// Update a memory row in place.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    content: Option<&str>,
    embedding: Option<Vec<f32>>,
    summary: Option<&str>,
    tags: Option<&[String]>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE memories SET
            content = COALESCE($2, content),
            embedding = COALESCE($3, embedding),
            summary = COALESCE($4, summary),
            tags = COALESCE($5, tags),
            updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(content)
    .bind(embedding.map(Vector::from))
    .bind(summary)
    .bind(tags)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Merge two ordinary memories after a transactional protection recheck.
///
/// Returns `false` without mutation if either row disappeared or became a
/// Plan, Rule, or workflow artifact after candidate discovery.
///
/// # Errors
///
/// Returns an error if locking or mutation fails.
pub async fn dream_merge(
    pool: &PgPool,
    survivor_id: Uuid,
    source_id: Uuid,
    content: &str,
    embedding: Vec<f32>,
    summary: &str,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    acquire_workflow_provenance_mutex(&mut transaction).await?;
    let ids = [survivor_id, source_id];
    let rows = sqlx::query(
        "SELECT id, category, workflow_artifact
         FROM memories
         WHERE id = ANY($1)
         ORDER BY id
         FOR UPDATE",
    )
    .bind(ids)
    .fetch_all(&mut *transaction)
    .await?;
    let mut protected = false;
    for row in &rows {
        protected |= memory_is_dream_protected(row)?;
    }
    if rows.len() != 2 || protected {
        transaction.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE memories
         SET content = $2, embedding = $3, summary = $4, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(survivor_id)
    .bind(content)
    .bind(Vector::from(embedding))
    .bind(summary)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(source_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(true)
}

/// Prune an ordinary memory after a transactional protection recheck.
///
/// # Errors
///
/// Returns an error if locking or deletion fails.
pub async fn dream_prune(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    acquire_workflow_provenance_mutex(&mut transaction).await?;
    let row = sqlx::query(
        "SELECT id, category, workflow_artifact
         FROM memories
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(row) = row else {
        transaction.commit().await?;
        return Ok(false);
    };
    if memory_is_dream_protected(&row)? {
        transaction.commit().await?;
        return Ok(false);
    }
    sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(true)
}

fn memory_is_dream_protected(row: &sqlx::postgres::PgRow) -> Result<bool, sqlx::Error> {
    let category = row.try_get::<Category, _>("category")?;
    let workflow_artifact = row.try_get::<bool, _>("workflow_artifact")?;
    Ok(workflow_artifact || matches!(category, Category::Plan | Category::Rule))
}

/// Update a memory while preserving and propagating workflow provenance.
///
/// # Errors
///
/// Returns an error if the transaction fails.
pub async fn update_with_workflow_provenance(
    pool: &PgPool,
    id: Uuid,
    content: Option<&str>,
    embedding: Option<Vec<f32>>,
    summary: Option<&str>,
    tags: Option<&[String]>,
) -> Result<bool, sqlx::Error> {
    let Some(tags) = tags else {
        return update(pool, id, content, embedding, summary, None).await;
    };
    // Category is immutable after insertion, so this preflight safely avoids the
    // global provenance mutex for memories that cannot participate in workflow
    // relationships. The transaction still reloads and locks the target row.
    let category = sqlx::query_scalar::<_, Category>("SELECT category FROM memories WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(category) = category else {
        return Ok(false);
    };
    if !matches!(category, Category::Plan | Category::Decision) {
        return update(pool, id, content, embedding, summary, Some(tags)).await;
    }

    let review_targets = workflow::reviewed_item_ids(tags);
    let mut transaction = pool.begin().await?;
    acquire_workflow_provenance_mutex(&mut transaction).await?;
    let current = sqlx::query(
        "SELECT category
         FROM memories
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(current) = current else {
        transaction.commit().await?;
        return Ok(false);
    };
    let category: Category = current.try_get("category")?;
    let directly_scoped = category == Category::Plan && workflow::plan_is_directly_scoped(tags);

    sqlx::query(
        "UPDATE memories SET
            content = COALESCE($2, content),
            embedding = COALESCE($3, embedding),
            summary = COALESCE($4, summary),
            tags = COALESCE($5, tags),
            workflow_artifact = workflow_artifact OR $6,
            updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(content)
    .bind(embedding.map(Vector::from))
    .bind(summary)
    .bind(Some(tags))
    .bind(directly_scoped)
    .execute(&mut *transaction)
    .await?;

    if matches!(category, Category::Plan | Category::Decision) {
        promote_workflow_relationships(&mut transaction, id, &category, &review_targets).await?;
    }
    transaction.commit().await?;
    Ok(true)
}

async fn acquire_workflow_provenance_mutex(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(WORKFLOW_PROVENANCE_MUTEX)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn acquire_external_session_mutex(
    transaction: &mut Transaction<'_, Postgres>,
    external_session_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(external_session_lock_key(external_session_id))
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn external_session_lock_key(external_session_id: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in b"memory-server:external-session:"
        .iter()
        .chain(external_session_id.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    i64::from_le_bytes(hash.to_le_bytes())
}

async fn reconcile_session_parents(
    transaction: &mut Transaction<'_, Postgres>,
    session_id: Uuid,
    log_id: Uuid,
    workflow_artifact: bool,
) -> Result<(), sqlx::Error> {
    if !workflow_artifact {
        return Ok(());
    }
    sqlx::query(
        "UPDATE sessions
         SET workflow_artifact = TRUE,
             updated_at = NOW()
         WHERE id = $1 AND workflow_artifact = FALSE",
    )
    .bind(session_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE session_logs
         SET workflow_artifact = TRUE
         WHERE id = $1 AND workflow_artifact = FALSE",
    )
    .bind(log_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn promote_workflow_relationships(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
    category: &Category,
    review_targets: &[Uuid],
) -> Result<(), sqlx::Error> {
    // The caller holds the provenance mutex and primary row. Only relationship
    // targets are subsequently locked, always before dependent decisions.
    match category {
        Category::Plan => promote_plan_lineage(transaction, id).await?,
        Category::Decision => {
            let marked_targets = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM memories
                 WHERE id = ANY($1) AND category = 'plan' AND workflow_artifact
                 ORDER BY id FOR UPDATE",
            )
            .bind(review_targets)
            .fetch_all(&mut **transaction)
            .await?;
            if !marked_targets.is_empty() {
                mark_workflow_memories(transaction, &[id]).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn promote_plan_lineage(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    // A previously missing ancestor can be inserted after its marked successor.
    // That incoming relationship is authoritative under the provenance mutex.
    let marked: bool = sqlx::query_scalar(
        "SELECT workflow_artifact OR EXISTS (
             SELECT 1 FROM memories
             WHERE category = 'plan' AND workflow_artifact
               AND workflow_relationship_ids(tags, 'supersedes-plan:') @> ARRAY[$1]::UUID[]
         ) FROM memories WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut **transaction)
    .await?;
    if !marked {
        return Ok(());
    }

    let mut seen = HashSet::new();
    let mut frontier = vec![id];
    let mut marked_plans = Vec::new();
    while !frontier.is_empty() {
        let rows = sqlx::query(
            "SELECT id, tags FROM memories
             WHERE category = 'plan' AND id = ANY($1)
             ORDER BY id FOR UPDATE",
        )
        .bind(&frontier)
        .fetch_all(&mut **transaction)
        .await?;
        frontier.clear();
        for row in rows {
            let plan_id: Uuid = row.try_get("id")?;
            if seen.insert(plan_id) {
                marked_plans.push(plan_id);
                let tags: Vec<String> = row.try_get("tags")?;
                frontier.extend(
                    workflow::superseded_plan_ids(&tags)
                        .into_iter()
                        .filter(|target| !seen.contains(target)),
                );
            }
        }
    }
    mark_workflow_memories(transaction, &marked_plans).await?;

    // Positive indexed UUID overlap finds only reviews of this lineage. It also
    // catches reviews written before their plan, across project boundaries.
    let decisions = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM memories
         WHERE category = 'decision' AND workflow_artifact = FALSE
           AND workflow_relationship_ids(tags, 'reviewed-item:') && $1::UUID[]
         ORDER BY id FOR UPDATE",
    )
    .bind(&marked_plans)
    .fetch_all(&mut **transaction)
    .await?;
    mark_workflow_memories(transaction, &decisions).await
}

async fn mark_workflow_memories(
    transaction: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    if !ids.is_empty() {
        sqlx::query(
            "UPDATE memories SET workflow_artifact = TRUE
             WHERE id = ANY($1) AND workflow_artifact = FALSE",
        )
        .bind(ids)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

pub struct HybridSearchParams<'a> {
    pub category: Option<&'a Category>,
    pub include_workflow_artifacts: bool,
    pub limit: i64,
    pub min_similarity: f64,
    pub project: &'a str,
    pub query: &'a str,
    pub tags: Option<&'a [String]>,
}

const HYBRID_CATEGORY_INCLUSIVE_SQL: &str = r"
    WITH vector_results AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
        FROM memories
        WHERE project = $2 AND category = $3
          AND 1 - (embedding <=> $1) >= $5
          AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
        ORDER BY embedding <=> $1
        LIMIT $4
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC
        ) AS rank_f
        FROM memories
        WHERE project = $2 AND category = $3
          AND fts @@ plainto_tsquery('english', $6)
          AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC
        LIMIT $4
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary,
           m.tags, m.updated_at, c.rrf_score AS similarity
    FROM combined c
    JOIN memories m ON m.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $7";

const HYBRID_CATEGORY_DEFAULT_SQL: &str = r"
    WITH vector_results AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
        FROM memories
        WHERE project = $2 AND category = $3
          AND workflow_artifact = FALSE
          AND 1 - (embedding <=> $1) >= $5
          AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
        ORDER BY embedding <=> $1
        LIMIT $4
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC
        ) AS rank_f
        FROM memories
        WHERE project = $2 AND category = $3
          AND workflow_artifact = FALSE
          AND fts @@ plainto_tsquery('english', $6)
          AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC
        LIMIT $4
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary,
           m.tags, m.updated_at, c.rrf_score AS similarity
    FROM combined c
    JOIN memories m ON m.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $7";

const HYBRID_INCLUSIVE_SQL: &str = r"
    WITH vector_results AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
        FROM memories
        WHERE project = $2
          AND 1 - (embedding <=> $1) >= $4
          AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
        ORDER BY embedding <=> $1
        LIMIT $3
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        ) AS rank_f
        FROM memories
        WHERE project = $2
          AND fts @@ plainto_tsquery('english', $5)
          AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        LIMIT $3
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary,
           m.tags, m.updated_at, c.rrf_score AS similarity
    FROM combined c
    JOIN memories m ON m.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $6";

const HYBRID_DEFAULT_SQL: &str = r"
    WITH vector_results AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
        FROM memories
        WHERE project = $2
          AND workflow_artifact = FALSE
          AND 1 - (embedding <=> $1) >= $4
          AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
        ORDER BY embedding <=> $1
        LIMIT $3
    ), fts_results AS (
        SELECT id, ROW_NUMBER() OVER (
            ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        ) AS rank_f
        FROM memories
        WHERE project = $2
          AND workflow_artifact = FALSE
          AND fts @@ plainto_tsquery('english', $5)
          AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
        ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC
        LIMIT $3
    ), combined AS (
        SELECT COALESCE(v.id, f.id) AS id,
               (COALESCE(1.0 / (60 + v.rank_v), 0)
                + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
        FROM vector_results v
        FULL OUTER JOIN fts_results f ON v.id = f.id
    )
    SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary,
           m.tags, m.updated_at, c.rrf_score AS similarity
    FROM combined c
    JOIN memories m ON m.id = c.id
    ORDER BY c.rrf_score DESC
    LIMIT $6";

/// Run hybrid semantic plus full-text memory search.
///
/// # Errors
///
/// Returns an error if the query fails.
#[allow(clippy::too_many_arguments)]
pub async fn hybrid_search(
    pool: &PgPool,
    embedding: Vec<f32>,
    params: HybridSearchParams<'_>,
) -> Result<Vec<(MemorySummary, f64)>, sqlx::Error> {
    let query_vec = Vector::from(embedding);
    let fetch_limit = params.limit * 3;
    let tag_arr = params.tags.map(<[String]>::to_vec);
    let rows = if let Some(cat) = params.category {
        let statement = if params.include_workflow_artifacts {
            HYBRID_CATEGORY_INCLUSIVE_SQL
        } else {
            HYBRID_CATEGORY_DEFAULT_SQL
        };
        sqlx::query(statement)
            .bind(&query_vec)
            .bind(params.project)
            .bind(cat)
            .bind(fetch_limit)
            .bind(params.min_similarity)
            .bind(params.query)
            .bind(params.limit)
            .bind(&tag_arr)
            .fetch_all(pool)
            .await?
    } else {
        let statement = if params.include_workflow_artifacts {
            HYBRID_INCLUSIVE_SQL
        } else {
            HYBRID_DEFAULT_SQL
        };
        sqlx::query(statement)
            .bind(&query_vec)
            .bind(params.project)
            .bind(fetch_limit)
            .bind(params.min_similarity)
            .bind(params.query)
            .bind(params.limit)
            .bind(&tag_arr)
            .fetch_all(pool)
            .await?
    };
    rows.iter()
        .map(|row| {
            let memory = row_to_summary(row)?;
            let similarity: f64 = row.try_get("similarity")?;
            Ok((memory, similarity))
        })
        .collect()
}

fn parse_pgvector_text(text: &str) -> Vec<f32> {
    text.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect()
}

fn row_to_edge_summary(row: &sqlx::postgres::PgRow) -> Result<MemoryEdgeSummary, sqlx::Error> {
    Ok(MemoryEdgeSummary {
        id: row.try_get("edge_id")?,
        confidence: row.try_get("confidence")?,
        created_at: row.try_get("edge_created_at")?,
        dst_id: row.try_get("dst_id")?,
        dst_project: row.try_get("dst_project")?,
        evidence: row.try_get("evidence")?,
        origin: row.try_get("origin")?,
        relation: row.try_get("relation")?,
        src_id: row.try_get("src_id")?,
        src_project: row.try_get("src_project")?,
        suppressed: row.try_get("suppressed")?,
        updated_at: row.try_get("edge_updated_at")?,
        weight: row.try_get("weight")?,
    })
}

fn row_to_session_log_summary(
    row: &sqlx::postgres::PgRow,
) -> Result<SessionLogSummary, sqlx::Error> {
    Ok(SessionLogSummary {
        id: row.try_get("id")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        cwd: row.try_get("cwd")?,
        project: row.try_get("project")?,
        session_id: row.try_get("session_id")?,
        summary: row.try_get("summary")?,
    })
}

fn row_to_session_message_summary(
    row: &sqlx::postgres::PgRow,
) -> Result<SessionMessageSummary, sqlx::Error> {
    Ok(SessionMessageSummary {
        agent: row.try_get("agent")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        metadata: row.try_get("metadata")?,
        role: row.try_get("role")?,
        session_id: row.try_get("session_id")?,
    })
}

fn row_to_session_summary(row: &sqlx::postgres::PgRow) -> Result<SessionSummary, sqlx::Error> {
    Ok(SessionSummary {
        agent: row.try_get("agent")?,
        created_at: row.try_get("created_at")?,
        cwd: row.try_get("cwd")?,
        ended_at: row.try_get("ended_at")?,
        external_session_id: row.try_get("external_session_id")?,
        id: row.try_get("id")?,
        project: row.try_get("project")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_summary(row: &sqlx::postgres::PgRow) -> Result<MemorySummary, sqlx::Error> {
    Ok(MemorySummary {
        id: row.try_get("id")?,
        category: row.try_get("category")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        project: row.try_get("project")?,
        summary: row.try_get("summary")?,
        tags: row.try_get("tags")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::model::{
        Category, EdgeOrigin, EdgeRelation, Memory, MemoryEdge, Session, SessionLog,
        SessionLogChunk, SessionMessage,
    };

    use super::*;

    fn test_memory(id: Uuid, project: &str) -> Memory {
        Memory {
            id,
            category: Category::Context,
            content: "test content".to_owned(),
            created_at: Utc::now(),
            embedding: vec![0.0; 1024],
            project: project.to_owned(),
            summary: "test summary".to_owned(),
            tags: vec![],
            updated_at: Utc::now(),
        }
    }

    fn workflow_memory(id: Uuid, category: Category, project: &str, tags: Vec<String>) -> Memory {
        Memory {
            category,
            tags,
            ..test_memory(id, project)
        }
    }

    async fn is_workflow_artifact(pool: &PgPool, id: Uuid) -> bool {
        sqlx::query_scalar("SELECT workflow_artifact FROM memories WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn test_session(external_session_id: &str) -> Session {
        let now = Utc::now();
        Session {
            agent: "test".to_owned(),
            created_at: now,
            cwd: "/tmp/project".to_owned(),
            ended_at: None,
            external_session_id: external_session_id.to_owned(),
            id: Uuid::new_v4(),
            project: "project".to_owned(),
            updated_at: now,
            workflow_artifact: false,
        }
    }

    fn test_session_log(external_session_id: &str, workflow_artifact: bool) -> SessionLog {
        SessionLog {
            id: Uuid::new_v4(),
            content: "transcript".to_owned(),
            created_at: Utc::now(),
            cwd: "/tmp/project".to_owned(),
            embedding: vec![0.0; 1024],
            project: "project".to_owned(),
            session_id: external_session_id.to_owned(),
            summary: "summary".to_owned(),
            workflow_artifact,
        }
    }

    fn test_session_chunk(log_id: Uuid, workflow_artifact: bool) -> SessionLogChunk {
        SessionLogChunk {
            chunk_index: 0,
            content: "transcript".to_owned(),
            embedding: vec![0.0; 1024],
            id: Uuid::new_v4(),
            session_log_id: log_id,
            workflow_artifact,
        }
    }

    async fn correlated_workflow_state(pool: &PgPool, external_session_id: &str) -> (bool, bool) {
        let session: bool = sqlx::query_scalar(
            "SELECT workflow_artifact
             FROM sessions
             WHERE external_session_id = $1",
        )
        .bind(external_session_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let log: bool = sqlx::query_scalar(
            "SELECT workflow_artifact
             FROM session_logs
             WHERE session_id = $1",
        )
        .bind(external_session_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (session, log)
    }

    async fn published_chunk_states(pool: &PgPool, external_session_id: &str) -> Vec<bool> {
        sqlx::query_scalar(
            "SELECT c.workflow_artifact
             FROM session_log_chunks c
             JOIN session_logs sl ON sl.id = c.session_log_id
             WHERE sl.session_id = $1
             ORDER BY c.chunk_index",
        )
        .bind(external_session_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[test]
    fn external_session_lock_keys_are_stable_and_namespaced() {
        assert_eq!(
            external_session_lock_key("session-a"),
            -6_739_915_711_213_144_206
        );
        assert_ne!(
            external_session_lock_key("session-a"),
            external_session_lock_key("session-b")
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn raw_publication_reloads_session_after_stale_absence(pool: PgPool) {
        stale_absence_publication(&pool, true).await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn ordinary_publication_reloads_session_after_stale_absence(pool: PgPool) {
        stale_absence_publication(&pool, false).await;
    }

    async fn stale_absence_publication(pool: &PgPool, task_marked: bool) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let external_id = Uuid::new_v4().to_string();
            let preparing_pool = pool.clone();
            let preparing_id = external_id.clone();
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
            let publisher = tokio::spawn(async move {
                let log = test_session_log(&preparing_id, false);
                let chunk = test_session_chunk(log.id, false);
                // Preparation observes absence before the DB publisher boundary.
                // This observation must never become authoritative provenance.
                let absent: bool = sqlx::query_scalar(
                    "SELECT NOT EXISTS (
                         SELECT 1 FROM sessions WHERE external_session_id = $1
                     )",
                )
                .bind(&preparing_id)
                .fetch_one(&preparing_pool)
                .await
                .unwrap();
                observed_tx.send(absent).unwrap();
                resume_rx.await.unwrap();
                publish_session_log(&preparing_pool, &log, &[chunk], None, None)
                    .await
                    .unwrap()
                    .unwrap();
            });
            assert!(observed_rx.await.unwrap());

            let session = test_session(&external_id);
            let session_id = create_session(pool, &session).await.unwrap().id;
            append_session_message(
                pool,
                &SessionMessage {
                    agent: "test".to_owned(),
                    content: if task_marked {
                        format!("working on task:{}", Uuid::new_v4())
                    } else {
                        "ordinary work".to_owned()
                    },
                    created_at: Utc::now(),
                    id: Uuid::new_v4(),
                    kind: "message".to_owned(),
                    metadata: None,
                    role: "user".to_owned(),
                    session_id,
                },
            )
            .await
            .unwrap()
            .unwrap();
            let no_log: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS (SELECT 1 FROM session_logs WHERE session_id = $1)",
            )
            .bind(&external_id)
            .fetch_one(pool)
            .await
            .unwrap();
            assert!(no_log);
            // Both normalized writes have fully committed while publication paused.
            resume_tx.send(()).unwrap();
            publisher.await.unwrap();

            assert_eq!(
                correlated_workflow_state(pool, &external_id).await,
                (task_marked, task_marked)
            );
            assert_eq!(
                published_chunk_states(pool, &external_id).await,
                [task_marked]
            );
            for inclusive in [false, true] {
                let results = session_log_search(
                    pool,
                    vec![0.0; 1024],
                    "transcript",
                    "project",
                    10,
                    0.0,
                    inclusive,
                )
                .await
                .unwrap();
                assert_eq!(
                    results.iter().any(|(log, _)| log.session_id == external_id),
                    inclusive || !task_marked,
                );
            }
        })
        .await
        .expect("paused preparation and publication must complete without deadlock");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn task_append_first_promotes_later_raw_publication(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let session = test_session(&external_id);
        let session_id = create_session(&pool, &session).await.unwrap().id;
        append_session_message(
            &pool,
            &SessionMessage {
                agent: "test".to_owned(),
                content: format!("task:{} completed before raw publication", Uuid::new_v4()),
                created_at: Utc::now(),
                id: Uuid::new_v4(),
                kind: "message".to_owned(),
                metadata: None,
                role: "user".to_owned(),
                session_id,
            },
        )
        .await
        .unwrap()
        .unwrap();

        let log = test_session_log(&external_id, false);
        publish_session_log(
            &pool,
            &log,
            &[test_session_chunk(log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (true, true)
        );
        assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn marked_session_promotes_unmarked_raw_publication(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let mut session = test_session(&external_id);
        session.workflow_artifact = true;
        create_session(&pool, &session).await.unwrap();

        let log = test_session_log(&external_id, false);
        publish_session_log(
            &pool,
            &log,
            &[test_session_chunk(log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (true, true)
        );
        assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn raw_reingestion_promotes_ordinary_publication(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let session = test_session(&external_id);
        create_session(&pool, &session).await.unwrap();

        let ordinary_log = test_session_log(&external_id, false);
        publish_session_log(
            &pool,
            &ordinary_log,
            &[test_session_chunk(ordinary_log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (false, false)
        );

        let workflow_log = test_session_log(&external_id, true);
        publish_session_log(
            &pool,
            &workflow_log,
            &[test_session_chunk(workflow_log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (true, true)
        );
        assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unmarked_raw_reingestion_never_clears_workflow_provenance(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let session = test_session(&external_id);
        create_session(&pool, &session).await.unwrap();

        let workflow_log = test_session_log(&external_id, true);
        publish_session_log(
            &pool,
            &workflow_log,
            &[test_session_chunk(workflow_log.id, true)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        let ordinary_log = test_session_log(&external_id, false);
        publish_session_log(
            &pool,
            &ordinary_log,
            &[test_session_chunk(ordinary_log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (true, true)
        );
        assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn task_append_before_finalization_preserves_provenance(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let session = test_session(&external_id);
        let session_id = create_session(&pool, &session).await.unwrap().id;
        append_session_message(
            &pool,
            &SessionMessage {
                agent: "test".to_owned(),
                content: format!("finalize after task:{}", Uuid::new_v4()),
                created_at: Utc::now(),
                id: Uuid::new_v4(),
                kind: "message".to_owned(),
                metadata: None,
                role: "user".to_owned(),
                session_id,
            },
        )
        .await
        .unwrap()
        .unwrap();

        let log = test_session_log(&external_id, false);
        publish_session_log(
            &pool,
            &log,
            &[test_session_chunk(log.id, false)],
            Some(session_id),
            Some(Utc::now()),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            correlated_workflow_state(&pool, &external_id).await,
            (true, true)
        );
        assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
        let ended_at: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT ended_at FROM sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ended_at.is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn task_append_after_finalization_promotes_published_chunks(pool: PgPool) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let external_id = Uuid::new_v4().to_string();
            let session = test_session(&external_id);
            let session_id = create_session(&pool, &session).await.unwrap().id;
            let log = test_session_log(&external_id, false);
            let ended_at = Utc::now();
            publish_session_log(
                &pool,
                &log,
                &[test_session_chunk(log.id, false)],
                Some(session_id),
                Some(ended_at),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                correlated_workflow_state(&pool, &external_id).await,
                (false, false)
            );
            assert_eq!(published_chunk_states(&pool, &external_id).await, [false]);

            append_session_message(
                &pool,
                &SessionMessage {
                    agent: "test".to_owned(),
                    content: format!("task:{} appended after finalization", Uuid::new_v4()),
                    created_at: Utc::now(),
                    id: Uuid::new_v4(),
                    kind: "message".to_owned(),
                    metadata: None,
                    role: "user".to_owned(),
                    session_id,
                },
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                correlated_workflow_state(&pool, &external_id).await,
                (true, true)
            );
            assert_eq!(published_chunk_states(&pool, &external_id).await, [true]);
            let stored_end: chrono::DateTime<Utc> =
                sqlx::query_scalar("SELECT ended_at FROM sessions WHERE id = $1")
                    .bind(session_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stored_end.timestamp_micros(), ended_at.timestamp_micros());
        })
        .await
        .expect("append after finalized publication must complete without deadlock");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_promotions_advance_session_updated_at(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let mut session = test_session(&external_id);
        session.updated_at -= chrono::Duration::hours(2);
        session.created_at = session.updated_at;
        let session_id = create_session(&pool, &session).await.unwrap().id;
        let original_updated_at = session.updated_at;
        append_session_message(
            &pool,
            &SessionMessage {
                agent: "test".to_owned(),
                content: format!("working on task:{}", Uuid::new_v4()),
                created_at: original_updated_at,
                id: Uuid::new_v4(),
                kind: "message".to_owned(),
                metadata: None,
                role: "user".to_owned(),
                session_id,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let append_updated_at: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(append_updated_at > original_updated_at);

        let raw_external_id = Uuid::new_v4().to_string();
        let mut raw_session = test_session(&raw_external_id);
        raw_session.updated_at -= chrono::Duration::hours(2);
        raw_session.created_at = raw_session.updated_at;
        let raw_session_id = create_session(&pool, &raw_session).await.unwrap().id;
        let raw_original_updated_at = raw_session.updated_at;
        let log = test_session_log(&raw_external_id, true);
        publish_session_log(&pool, &log, &[test_session_chunk(log.id, true)], None, None)
            .await
            .unwrap()
            .unwrap();
        let raw_updated_at: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = $1")
                .bind(raw_session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(raw_updated_at > raw_original_updated_at);

        let correlated_external_id = Uuid::new_v4().to_string();
        let correlated_session = test_session(&correlated_external_id);
        let correlated_session_id = create_session(&pool, &correlated_session).await.unwrap().id;
        let correlated_log = test_session_log(&correlated_external_id, false);
        publish_session_log(
            &pool,
            &correlated_log,
            &[test_session_chunk(correlated_log.id, false)],
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let stale_updated_at = Utc::now() - chrono::Duration::hours(2);
        sqlx::query("UPDATE sessions SET updated_at = $2 WHERE id = $1")
            .bind(correlated_session_id)
            .bind(stale_updated_at)
            .execute(&pool)
            .await
            .unwrap();
        let append_started_at = Utc::now();
        append_session_message(
            &pool,
            &SessionMessage {
                agent: "test".to_owned(),
                content: format!("delayed task:{}", Uuid::new_v4()),
                created_at: stale_updated_at,
                id: Uuid::new_v4(),
                kind: "message".to_owned(),
                metadata: None,
                role: "user".to_owned(),
                session_id: correlated_session_id,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let correlated_updated_at: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = $1")
                .bind(correlated_session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(correlated_updated_at >= append_started_at);
        assert_eq!(
            correlated_workflow_state(&pool, &correlated_external_id).await,
            (true, true)
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_session_waits_for_absent_key_publisher(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let mut blocker = pool.begin().await.unwrap();
        acquire_external_session_mutex(&mut blocker, &external_id)
            .await
            .unwrap();

        let waiting_pool = pool.clone();
        let waiting_session = test_session(&external_id);
        let mut task =
            tokio::spawn(async move { create_session(&waiting_pool, &waiting_session).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut task)
                .await
                .is_err()
        );
        blocker.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn publication_rolls_back_parent_when_chunk_insert_fails(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let first_log = test_session_log(&external_id, false);
        let first_chunk = test_session_chunk(first_log.id, false);
        let stored_id = publish_session_log(&pool, &first_log, &[first_chunk], None, None)
            .await
            .unwrap()
            .unwrap();

        let mut replacement = test_session_log(&external_id, true);
        replacement.content = "replacement".to_owned();
        let mut invalid_chunk = test_session_chunk(replacement.id, true);
        invalid_chunk.embedding = vec![0.0; 2];
        assert!(
            publish_session_log(&pool, &replacement, &[invalid_chunk], None, None)
                .await
                .is_err()
        );
        let state: (String, bool, i64) = sqlx::query_as(
            "SELECT content, workflow_artifact,
                    (SELECT COUNT(*) FROM session_log_chunks WHERE session_log_id = $1)
             FROM session_logs
             WHERE id = $1",
        )
        .bind(stored_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, ("transcript".to_owned(), false, 1));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn composite_constraint_rejects_mismatched_chunk_provenance(pool: PgPool) {
        let external_id = Uuid::new_v4().to_string();
        let log = test_session_log(&external_id, true);
        let chunk = test_session_chunk(log.id, true);
        let stored_id = publish_session_log(&pool, &log, &[chunk], None, None)
            .await
            .unwrap()
            .unwrap();
        let result = sqlx::query(
            "INSERT INTO session_log_chunks
                (id, chunk_index, content, embedding, session_log_id,
                 workflow_artifact)
             VALUES ($1, 1, 'bad', $2, $3, FALSE)",
        )
        .bind(Uuid::new_v4())
        .bind(Vector::from(vec![0.0; 1024]))
        .bind(stored_id)
        .execute(&pool)
        .await;
        assert!(result.is_err());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_migration_backfills_historical_rows_idempotently(pool: PgPool) {
        const WORKFLOW_MIGRATION: &str =
            include_str!("../../migrations/20260810000000_add_workflow_artifact.sql");

        sqlx::raw_sql(
            "ALTER TABLE session_log_chunks
                 DROP CONSTRAINT session_log_chunks_parent_workflow_artifact_fkey;
             ALTER TABLE session_logs
                 DROP CONSTRAINT session_logs_id_workflow_artifact_key;
             ALTER TABLE session_log_chunks
                 ADD CONSTRAINT session_log_chunks_session_log_id_fkey
                 FOREIGN KEY (session_log_id) REFERENCES session_logs(id)
                 ON DELETE CASCADE;
             DROP INDEX idx_memories_tags;
             ALTER TABLE session_log_chunks DROP COLUMN workflow_artifact;
             ALTER TABLE session_logs DROP COLUMN workflow_artifact;
             ALTER TABLE sessions DROP COLUMN workflow_artifact;
             ALTER TABLE memories DROP COLUMN workflow_artifact;",
        )
        .execute(&pool)
        .await
        .unwrap();

        let task_id = Uuid::new_v4();
        let oldest_plan_id = Uuid::new_v4();
        let prior_plan_id = Uuid::new_v4();
        let current_plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();
        for memory in [
            workflow_memory(oldest_plan_id, Category::Plan, "old", vec![]),
            workflow_memory(
                prior_plan_id,
                Category::Plan,
                "prior",
                vec![format!("supersedes-plan:{oldest_plan_id}")],
            ),
            workflow_memory(
                current_plan_id,
                Category::Plan,
                "current",
                vec![
                    format!("task:{task_id}"),
                    format!("supersedes-plan:{prior_plan_id}"),
                ],
            ),
            workflow_memory(
                review_id,
                Category::Decision,
                "cross-project-review",
                vec![format!("reviewed-item:{current_plan_id}")],
            ),
        ] {
            insert(&pool, &memory).await.unwrap();
        }

        // Every accepted UUID spelling must agree with runtime classification,
        // for both direct marker prefixes, recursive edges, and review targets.
        let mut spelling_ids = Vec::new();
        for spelling in 0..5 {
            let render = |id: Uuid| match spelling {
                0 => id.to_string(),
                1 => id.to_string().to_uppercase(),
                2 => id.simple().to_string(),
                3 => id.braced().to_string(),
                _ => id.urn().to_string(),
            };
            for prefix in ["task:", "superseded-for-task:"] {
                let ancestor = Uuid::new_v4();
                let plan = Uuid::new_v4();
                let review = Uuid::new_v4();
                for memory in [
                    workflow_memory(ancestor, Category::Plan, "ancestor", vec![]),
                    workflow_memory(
                        plan,
                        Category::Plan,
                        "plan",
                        vec![
                            format!("{prefix}{}", render(task_id)),
                            format!("supersedes-plan:{}", render(ancestor)),
                        ],
                    ),
                    workflow_memory(
                        review,
                        Category::Decision,
                        "other-project",
                        vec![format!("reviewed-item:{}", render(ancestor))],
                    ),
                ] {
                    insert(&pool, &memory).await.unwrap();
                }
                spelling_ids.extend([ancestor, plan, review]);
            }
        }
        let ordinary_id = Uuid::new_v4();
        insert(
            &pool,
            &workflow_memory(
                ordinary_id,
                Category::Plan,
                "ordinary",
                vec![
                    "superseded-plan".to_owned(),
                    "task:not-a-uuid".to_owned(),
                    format!("prefix-task:{task_id}"),
                    format!("task:{task_id}:suffix"),
                ],
            ),
        )
        .await
        .unwrap();

        let message_external_id = Uuid::new_v4().to_string();
        let message_session_id = Uuid::new_v4();
        let log_external_id = Uuid::new_v4().to_string();
        let log_session_id = Uuid::new_v4();
        let message_log_id = Uuid::new_v4();
        let direct_log_id = Uuid::new_v4();
        let ordinary_log_id = Uuid::new_v4();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions
                (id, created_at, updated_at, cwd, project, external_session_id, agent)
             VALUES ($1, $5, $5, '/tmp', 'project', $2, 'test'),
                    ($3, $5, $5, '/tmp', 'project', $4, 'test')",
        )
        .bind(message_session_id)
        .bind(&message_external_id)
        .bind(log_session_id)
        .bind(&log_external_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_messages
                (id, session_id, created_at, agent, role, kind, content)
             VALUES ($1, $2, $3, 'test', 'user', 'message', $4)",
        )
        .bind(Uuid::new_v4())
        .bind(message_session_id)
        .bind(now)
        .bind(format!("historical task:{task_id}"))
        .execute(&pool)
        .await
        .unwrap();
        let zero_vector = Vector::from(vec![0.0; 1024]);
        sqlx::query(
            "INSERT INTO session_logs
                (id, content, created_at, cwd, embedding, project, session_id, summary)
             VALUES ($1, 'ordinary transcript', $7, '/tmp', $8, 'project', $2, 'ordinary'),
                    ($3, $9, $7, '/tmp', $8, 'project', $4, 'direct'),
                    ($5, 'ordinary transcript', $7, '/tmp', $8, 'project', $6, 'ordinary')",
        )
        .bind(message_log_id)
        .bind(&message_external_id)
        .bind(direct_log_id)
        .bind(&log_external_id)
        .bind(ordinary_log_id)
        .bind(Uuid::new_v4().to_string())
        .bind(now)
        .bind(&zero_vector)
        .bind(format!("published task:{task_id}"))
        .execute(&pool)
        .await
        .unwrap();
        for (index, log_id) in [message_log_id, direct_log_id, ordinary_log_id]
            .into_iter()
            .enumerate()
        {
            sqlx::query(
                "INSERT INTO session_log_chunks
                    (id, chunk_index, content, embedding, session_log_id)
                 VALUES ($1, $2, 'chunk', $3, $4)",
            )
            .bind(Uuid::new_v4())
            .bind(i32::try_from(index).unwrap())
            .bind(&zero_vector)
            .bind(log_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::raw_sql(WORKFLOW_MIGRATION)
            .execute(&pool)
            .await
            .unwrap();

        let marked_memories: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM memories WHERE workflow_artifact ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        for id in [oldest_plan_id, prior_plan_id, current_plan_id, review_id] {
            assert!(marked_memories.contains(&id));
        }
        for id in spelling_ids {
            assert!(
                marked_memories.contains(&id),
                "UUID spelling left {id} unmarked"
            );
        }
        assert!(!marked_memories.contains(&ordinary_id));
        let session_states: Vec<(Uuid, bool)> =
            sqlx::query_as("SELECT id, workflow_artifact FROM sessions ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(session_states.contains(&(message_session_id, true)));
        assert!(session_states.contains(&(log_session_id, true)));
        let log_states: Vec<(Uuid, bool)> =
            sqlx::query_as("SELECT id, workflow_artifact FROM session_logs ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(log_states.contains(&(message_log_id, true)));
        assert!(log_states.contains(&(direct_log_id, true)));
        assert!(log_states.contains(&(ordinary_log_id, false)));
        let chunk_states: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT session_log_id, workflow_artifact
             FROM session_log_chunks
             ORDER BY session_log_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(chunk_states.contains(&(message_log_id, true)));
        assert!(chunk_states.contains(&(direct_log_id, true)));
        assert!(chunk_states.contains(&(ordinary_log_id, false)));

        let backfill_start = WORKFLOW_MIGRATION.find("WITH RECURSIVE").unwrap();
        let backfill_end = WORKFLOW_MIGRATION
            .find("ALTER TABLE session_logs\n    ADD CONSTRAINT")
            .unwrap();
        let rerun = sqlx::raw_sql(&WORKFLOW_MIGRATION[backfill_start..backfill_end])
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(rerun.rows_affected(), 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_provenance_propagates_and_never_clears(pool: PgPool) {
        let task_id = Uuid::new_v4();
        let old_plan_id = Uuid::new_v4();
        let intermediate_plan_id = Uuid::new_v4();
        let current_plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();

        let tags_index_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                 FROM pg_indexes
                 WHERE schemaname = current_schema()
                   AND tablename = 'memories'
                   AND indexname = 'idx_memories_tags'
             )",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(tags_index_exists);

        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                review_id,
                Category::Decision,
                "review-project",
                vec![format!("reviewed-item:{current_plan_id}")],
            ),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(old_plan_id, Category::Plan, "old-project", vec![]),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                intermediate_plan_id,
                Category::Plan,
                "intermediate-project",
                vec![format!("supersedes-plan:{old_plan_id}")],
            ),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                current_plan_id,
                Category::Plan,
                "current-project",
                vec![
                    format!("task:{task_id}"),
                    format!("supersedes-plan:{intermediate_plan_id}"),
                ],
            ),
        )
        .await
        .unwrap();

        assert!(is_workflow_artifact(&pool, current_plan_id).await);
        assert!(is_workflow_artifact(&pool, intermediate_plan_id).await);
        assert!(is_workflow_artifact(&pool, old_plan_id).await);
        assert!(is_workflow_artifact(&pool, review_id).await);

        update_with_workflow_provenance(&pool, current_plan_id, None, None, None, Some(&[]))
            .await
            .unwrap();
        assert!(is_workflow_artifact(&pool, current_plan_id).await);
        assert!(is_workflow_artifact(&pool, intermediate_plan_id).await);
        assert!(is_workflow_artifact(&pool, old_plan_id).await);
        assert!(is_workflow_artifact(&pool, review_id).await);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn relationship_index_parser_matches_rust(pool: PgPool) {
        let id = Uuid::new_v4();
        let spellings = [
            id.to_string(),
            id.to_string().to_uppercase(),
            id.simple().to_string(),
            id.braced().to_string(),
            id.urn().to_string(),
            format!("{id}:extra"),
            format!(" {id}"),
            format!("URN:UUID:{id}"),
            "not-a-uuid".to_owned(),
        ];
        for prefix in ["reviewed-item:", "supersedes-plan:"] {
            for spelling in &spellings {
                let tags = vec![format!("{prefix}{spelling}"), format!("x-{prefix}{id}")];
                let actual: Vec<Uuid> = sqlx::query_scalar(
                    "SELECT array_remove(workflow_relationship_ids($1, $2), NULL)",
                )
                .bind(&tags)
                .bind(prefix)
                .fetch_one(&pool)
                .await
                .unwrap();
                let expected = if prefix == "reviewed-item:" {
                    workflow::reviewed_item_ids(&tags)
                } else {
                    workflow::superseded_plan_ids(&tags)
                };
                assert_eq!(actual, expected, "{tags:?}");
            }
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_writes_do_not_lock_unrelated_rows(pool: PgPool) {
        let unrelated_plan = Uuid::new_v4();
        let unrelated_review = Uuid::new_v4();
        for (id, category) in [
            (unrelated_plan, Category::Plan),
            (unrelated_review, Category::Decision),
        ] {
            insert_with_workflow_provenance(
                &pool,
                &workflow_memory(id, category, "unrelated", vec![]),
            )
            .await
            .unwrap();
        }
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM memories WHERE id = ANY($1) FOR UPDATE")
            .bind([unrelated_plan, unrelated_review])
            .fetch_all(&mut *holder)
            .await
            .unwrap();

        let plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();
        // The old table-wide FOR UPDATE blocks here until this timeout.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            insert_with_workflow_provenance(
                &pool,
                &workflow_memory(plan_id, Category::Plan, "plan", vec![]),
            )
            .await
            .unwrap();
            insert_with_workflow_provenance(
                &pool,
                &workflow_memory(
                    review_id,
                    Category::Decision,
                    "review",
                    vec![format!("reviewed-item:{plan_id}")],
                ),
            )
            .await
            .unwrap();
            update_with_workflow_provenance(
                &pool,
                plan_id,
                None,
                None,
                None,
                Some(&[format!("task:{}", Uuid::new_v4())]),
            )
            .await
            .unwrap();
            let later_review_id = Uuid::new_v4();
            insert_with_workflow_provenance(
                &pool,
                &workflow_memory(
                    later_review_id,
                    Category::Decision,
                    "later-review",
                    vec![format!("reviewed-item:{plan_id}")],
                ),
            )
            .await
            .unwrap();
            assert!(is_workflow_artifact(&pool, later_review_id).await);
        })
        .await
        .expect("unrelated row locks must not block workflow writes");
        holder.rollback().await.unwrap();
        assert!(is_workflow_artifact(&pool, plan_id).await);
        assert!(is_workflow_artifact(&pool, review_id).await);
        assert!(!is_workflow_artifact(&pool, unrelated_plan).await);
        assert!(!is_workflow_artifact(&pool, unrelated_review).await);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn late_ancestor_and_new_edges_on_marked_plan_propagate(pool: PgPool) {
        let current = Uuid::new_v4();
        let prior = Uuid::new_v4();
        let oldest = Uuid::new_v4();
        let review = Uuid::new_v4();
        for memory in [
            workflow_memory(
                review,
                Category::Decision,
                "review",
                vec![format!("reviewed-item:{}", prior.simple())],
            ),
            workflow_memory(
                current,
                Category::Plan,
                "current",
                vec![
                    format!("task:{}", Uuid::new_v4()),
                    format!("supersedes-plan:{}", prior.to_string().to_uppercase()),
                ],
            ),
            workflow_memory(prior, Category::Plan, "prior", vec![]),
            workflow_memory(oldest, Category::Plan, "oldest", vec![]),
        ] {
            insert_with_workflow_provenance(&pool, &memory)
                .await
                .unwrap();
        }
        assert!(is_workflow_artifact(&pool, prior).await);
        assert!(is_workflow_artifact(&pool, review).await);
        assert!(!is_workflow_artifact(&pool, oldest).await);
        // Removing direct evidence and adding an edge must still propagate from
        // durable provenance. A cycle must terminate without losing any row.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            update_with_workflow_provenance(
                &pool,
                prior,
                None,
                None,
                None,
                Some(&[
                    format!("supersedes-plan:{oldest}"),
                    format!("supersedes-plan:{current}"),
                ]),
            )
            .await
            .unwrap();
        })
        .await
        .unwrap();
        assert!(is_workflow_artifact(&pool, oldest).await);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn updates_without_workflow_tag_relationships_skip_provenance_mutex(pool: PgPool) {
        let plan_id = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(plan_id, Category::Plan, "project", vec![]),
        )
        .await
        .unwrap();
        insert(&pool, &test_memory(context_id, "project"))
            .await
            .unwrap();

        let mut lock_holder = pool.begin().await.unwrap();
        acquire_workflow_provenance_mutex(&mut lock_holder)
            .await
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            update_with_workflow_provenance(
                &pool,
                plan_id,
                None,
                None,
                Some("content-only update"),
                None,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            update_with_workflow_provenance(
                &pool,
                context_id,
                None,
                None,
                None,
                Some(&["ordinary-tag".to_owned()]),
            ),
        )
        .await
        .unwrap()
        .unwrap();

        lock_holder.rollback().await.unwrap();
        assert_eq!(
            get(&pool, plan_id).await.unwrap().unwrap().summary,
            "content-only update"
        );
        assert_eq!(
            get(&pool, context_id).await.unwrap().unwrap().tags,
            ["ordinary-tag"]
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn supersedes_relationship_does_not_mark_non_plan_targets(pool: PgPool) {
        let non_plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();
        let plan_id = Uuid::new_v4();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(non_plan_id, Category::Decision, "other", vec![]),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                review_id,
                Category::Decision,
                "review",
                vec![format!("reviewed-item:{non_plan_id}")],
            ),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                plan_id,
                Category::Plan,
                "plan",
                vec![
                    format!("task:{}", Uuid::new_v4()),
                    format!("supersedes-plan:{non_plan_id}"),
                ],
            ),
        )
        .await
        .unwrap();

        assert!(is_workflow_artifact(&pool, plan_id).await);
        assert!(!is_workflow_artifact(&pool, non_plan_id).await);
        assert!(!is_workflow_artifact(&pool, review_id).await);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn concurrent_plan_and_review_inserts_reconcile(pool: PgPool) {
        let task_id = Uuid::new_v4();
        let plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();
        let plan = workflow_memory(
            plan_id,
            Category::Plan,
            "plan-project",
            vec![format!("task:{task_id}")],
        );
        let review = workflow_memory(
            review_id,
            Category::Decision,
            "review-project",
            vec![format!("reviewed-item:{plan_id}")],
        );
        let plan_pool = pool.clone();
        let review_pool = pool.clone();
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let (plan_result, review_result) = tokio::join!(
                insert_with_workflow_provenance(&plan_pool, &plan),
                insert_with_workflow_provenance(&review_pool, &review),
            );
            plan_result.unwrap();
            review_result.unwrap();
        })
        .await
        .unwrap();

        assert!(is_workflow_artifact(&pool, plan_id).await);
        assert!(is_workflow_artifact(&pool, review_id).await);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn dream_candidates_and_mutations_preserve_workflow_reviews(pool: PgPool) {
        let plan_id = Uuid::new_v4();
        let review_id = Uuid::new_v4();
        let ordinary_id = Uuid::new_v4();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(plan_id, Category::Plan, "project", vec![]),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                review_id,
                Category::Decision,
                "project",
                vec![format!("reviewed-item:{plan_id}")],
            ),
        )
        .await
        .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(ordinary_id, Category::Decision, "project", vec![]),
        )
        .await
        .unwrap();

        let before = maintenance_embeddings(&pool, "project").await.unwrap();
        assert!(before.iter().any(|(id, _)| *id == review_id));
        update_with_workflow_provenance(
            &pool,
            plan_id,
            None,
            None,
            None,
            Some(&[format!("task:{}", Uuid::new_v4())]),
        )
        .await
        .unwrap();
        assert!(is_workflow_artifact(&pool, review_id).await);

        assert!(
            !dream_merge(
                &pool,
                ordinary_id,
                review_id,
                "merged content",
                vec![1.0; 1024],
                "merged summary",
            )
            .await
            .unwrap()
        );
        assert!(!dream_prune(&pool, review_id).await.unwrap());
        assert!(get(&pool, ordinary_id).await.unwrap().is_some());
        assert!(get(&pool, review_id).await.unwrap().is_some());

        let after = maintenance_embeddings(&pool, "project").await.unwrap();
        assert!(!after.iter().any(|(id, _)| *id == review_id));
        assert!(after.iter().any(|(id, _)| *id == ordinary_id));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn semantic_windows_filter_workflow_artifacts_before_limits(pool: PgPool) {
        let seed_id = Uuid::new_v4();
        let ordinary_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        insert(&pool, &test_memory(seed_id, "project"))
            .await
            .unwrap();
        insert(&pool, &test_memory(ordinary_id, "project"))
            .await
            .unwrap();
        insert_with_workflow_provenance(
            &pool,
            &workflow_memory(
                workflow_id,
                Category::Plan,
                "project",
                vec![format!("task:{}", Uuid::new_v4())],
            ),
        )
        .await
        .unwrap();

        upsert_edge(
            &pool,
            &test_edge(
                seed_id,
                "project",
                workflow_id,
                "project",
                EdgeRelation::Similar,
                EdgeOrigin::EmbeddingNeighbor,
                1.0,
            ),
        )
        .await
        .unwrap();
        upsert_edge(
            &pool,
            &test_edge(
                seed_id,
                "project",
                ordinary_id,
                "project",
                EdgeRelation::Similar,
                EdgeOrigin::EmbeddingNeighbor,
                0.9,
            ),
        )
        .await
        .unwrap();
        let default_neighbors = list_search_neighbors(&pool, seed_id, 1, false)
            .await
            .unwrap();
        assert_eq!(default_neighbors.len(), 1);
        assert_eq!(default_neighbors[0].1.id, ordinary_id);
        let inclusive_neighbors = list_search_neighbors(&pool, seed_id, 1, true)
            .await
            .unwrap();
        assert_eq!(inclusive_neighbors[0].1.id, workflow_id);

        let default_memories = hybrid_search(
            &pool,
            vec![0.0; 1024],
            HybridSearchParams {
                category: None,
                include_workflow_artifacts: false,
                limit: 10,
                min_similarity: 0.0,
                project: "project",
                query: "test",
                tags: None,
            },
        )
        .await
        .unwrap();
        assert!(
            default_memories
                .iter()
                .all(|(memory, _)| memory.id != workflow_id)
        );
        let inclusive_memories = hybrid_search(
            &pool,
            vec![0.0; 1024],
            HybridSearchParams {
                category: None,
                include_workflow_artifacts: true,
                limit: 10,
                min_similarity: 0.0,
                project: "project",
                query: "test",
                tags: None,
            },
        )
        .await
        .unwrap();
        assert!(
            inclusive_memories
                .iter()
                .any(|(memory, _)| memory.id == workflow_id)
        );

        let ordinary_log = test_session_log("ordinary-log", false);
        publish_session_log(
            &pool,
            &ordinary_log,
            &[test_session_chunk(ordinary_log.id, false)],
            None,
            None,
        )
        .await
        .unwrap();
        let workflow_log = test_session_log("workflow-log", true);
        publish_session_log(
            &pool,
            &workflow_log,
            &[test_session_chunk(workflow_log.id, true)],
            None,
            None,
        )
        .await
        .unwrap();
        let default_logs = session_log_search(
            &pool,
            vec![0.0; 1024],
            "transcript",
            "project",
            10,
            0.0,
            false,
        )
        .await
        .unwrap();
        assert_eq!(default_logs.len(), 1);
        assert_eq!(default_logs[0].0.session_id, "ordinary-log");
        let inclusive_logs = session_log_search(
            &pool,
            vec![0.0; 1024],
            "transcript",
            "project",
            10,
            0.0,
            true,
        )
        .await
        .unwrap();
        assert_eq!(inclusive_logs.len(), 2);
    }

    fn test_edge(
        src_id: Uuid,
        src_project: &str,
        dst_id: Uuid,
        dst_project: &str,
        relation: EdgeRelation,
        origin: EdgeOrigin,
        weight: f64,
    ) -> MemoryEdge {
        let now = Utc::now();
        MemoryEdge {
            id: Uuid::new_v4(),
            confidence: 1.0,
            created_at: now,
            dst_id,
            dst_project: dst_project.to_owned(),
            evidence: None,
            origin,
            relation,
            src_id,
            src_project: src_project.to_owned(),
            suppressed: false,
            updated_at: now,
            weight,
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_neighbors_dedup_symmetric_same_edge(pool: PgPool) {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        insert(&pool, &test_memory(id_a, "test")).await.unwrap();
        insert(&pool, &test_memory(id_b, "test")).await.unwrap();

        // Insert symmetric pair: A→B and B→A with same relation+origin.
        let edge_ab = test_edge(
            id_a,
            "test",
            id_b,
            "test",
            EdgeRelation::Similar,
            EdgeOrigin::EmbeddingNeighbor,
            0.8,
        );
        let edge_ba = test_edge(
            id_b,
            "test",
            id_a,
            "test",
            EdgeRelation::Similar,
            EdgeOrigin::EmbeddingNeighbor,
            0.8,
        );
        upsert_edge(&pool, &edge_ab).await.unwrap();
        upsert_edge(&pool, &edge_ba).await.unwrap();

        // Should return B exactly once from A's perspective.
        let neighbors = list_neighbors(&pool, id_a, 20).await.unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].1.id, id_b);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_neighbors_preserves_distinct_edge_types(pool: PgPool) {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        insert(&pool, &test_memory(id_a, "test")).await.unwrap();
        insert(&pool, &test_memory(id_b, "test")).await.unwrap();

        // Insert three edge types between the same pair, each with symmetric reverse.
        for (relation, origin) in [
            (EdgeRelation::References, EdgeOrigin::ContentUuidRef),
            (EdgeRelation::Similar, EdgeOrigin::EmbeddingNeighbor),
            (EdgeRelation::RelatedTag, EdgeOrigin::SharedTag),
        ] {
            let forward = test_edge(
                id_a,
                "test",
                id_b,
                "test",
                relation.clone(),
                origin.clone(),
                0.8,
            );
            let reverse = test_edge(id_b, "test", id_a, "test", relation, origin, 0.8);
            upsert_edge(&pool, &forward).await.unwrap();
            upsert_edge(&pool, &reverse).await.unwrap();
        }

        // Should return 3 edges (one per relation+origin), not 6 or 1.
        let neighbors = list_neighbors(&pool, id_a, 20).await.unwrap();
        assert_eq!(neighbors.len(), 3);
        assert!(neighbors.iter().all(|(_, m)| m.id == id_b));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_rules_shadows_general_correctly(pool: PgPool) {
        let mut general_rule = test_memory(Uuid::new_v4(), crate::app::GENERAL_RULE_PROJECT);
        general_rule.category = Category::Rule;
        general_rule.tags = vec!["lang:rust".to_owned()];

        let mut project_rule = test_memory(Uuid::new_v4(), "test-project");
        project_rule.category = Category::Rule;
        project_rule.tags = vec!["lang:rust".to_owned()];

        insert(&pool, &general_rule).await.unwrap();
        insert(&pool, &project_rule).await.unwrap();

        let tags = vec!["lang:rust".to_owned()];

        // 1. shadow_general = true (project matching rule shadows general)
        let rules = list_rules(&pool, "test-project", true, true, Some(&tags))
            .await
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, project_rule.id);

        // 2. Fallback: project has no matching rules, general does
        let mut general_only_rule = test_memory(Uuid::new_v4(), crate::app::GENERAL_RULE_PROJECT);
        general_only_rule.category = Category::Rule;
        general_only_rule.tags = vec!["lang:go".to_owned()];
        insert(&pool, &general_only_rule).await.unwrap();

        let go_tags = vec!["lang:go".to_owned()];
        let rules = list_rules(&pool, "test-project", true, true, Some(&go_tags))
            .await
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, general_only_rule.id);

        // 3. shadow_general = false (bootstrap mode includes both)
        let rules = list_rules(&pool, "test-project", true, false, Some(&tags))
            .await
            .unwrap();
        assert_eq!(rules.len(), 2);
    }
}
