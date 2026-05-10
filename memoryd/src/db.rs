use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::model::{
    Category, Memory, MemoryEdge, MemoryEdgeSummary, MemorySummary, Session, SessionLog,
    SessionLogChunk, SessionLogSummary, SessionMessage, SessionMessageSummary, SessionSummary,
};

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
    let row = sqlx::query(
        "INSERT INTO sessions (id, created_at, updated_at, ended_at, cwd, project, external_session_id, agent)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (external_session_id) DO UPDATE SET
            updated_at = EXCLUDED.updated_at,
            cwd = EXCLUDED.cwd,
            project = EXCLUDED.project,
            agent = CASE
                WHEN EXCLUDED.agent = '' THEN sessions.agent
                ELSE EXCLUDED.agent
            END
         RETURNING id, created_at, updated_at, ended_at, cwd, project, external_session_id, agent",
    )
    .bind(session.id)
    .bind(session.created_at)
    .bind(session.updated_at)
    .bind(session.ended_at)
    .bind(&session.cwd)
    .bind(&session.project)
    .bind(&session.external_session_id)
    .bind(&session.agent)
    .fetch_one(pool)
    .await?;
    row_to_session_summary(&row)
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
) -> Result<SessionMessageSummary, sqlx::Error> {
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
    .fetch_one(pool)
    .await?;

    sqlx::query(
        "UPDATE sessions
         SET updated_at = GREATEST(updated_at, $2)
         WHERE id = $1",
    )
    .bind(message.session_id)
    .bind(message.created_at)
    .execute(pool)
    .await?;

    row_to_session_message_summary(&row)
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
) -> Result<Vec<(SessionLogSummary, f64)>, sqlx::Error> {
    let query_vec = Vector::from(embedding);
    let fetch_limit = limit * 3;
    let rows = sqlx::query(
        "WITH nearest_chunks AS (
            SELECT c.session_log_id,
                   c.embedding <=> $1 AS distance
            FROM session_log_chunks c
            JOIN session_logs sl ON sl.id = c.session_log_id
            WHERE sl.project = $2
            ORDER BY c.embedding <=> $1
            LIMIT $3
        ),
        vector_results AS (
            SELECT session_log_id AS id,
                   ROW_NUMBER() OVER (ORDER BY MIN(distance)) AS rank_v
            FROM nearest_chunks
            WHERE 1 - distance >= $4
            GROUP BY session_log_id
        ),
        fts_results AS (
            SELECT id, ROW_NUMBER() OVER (ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC) AS rank_f
            FROM session_logs
            WHERE project = $2
              AND fts @@ plainto_tsquery('english', $5)
            LIMIT $3
        ),
        combined AS (
            SELECT COALESCE(v.id, f.id) AS id,
                   (COALESCE(1.0 / (60 + v.rank_v), 0) + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
            FROM vector_results v
            FULL OUTER JOIN fts_results f ON v.id = f.id
        )
        SELECT s.id, s.content, s.created_at, s.cwd, s.project, s.session_id, s.summary,
               c.rrf_score AS similarity
        FROM combined c
        JOIN session_logs s ON s.id = c.id
        ORDER BY c.rrf_score DESC
        LIMIT $6",
    )
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

/// Replace all chunks for a stored session log.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn session_log_chunks_replace(
    pool: &PgPool,
    session_log_id: Uuid,
    chunks: &[SessionLogChunk],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM session_log_chunks WHERE session_log_id = $1")
        .bind(session_log_id)
        .execute(pool)
        .await?;

    for chunk in chunks {
        let embedding = Vector::from(chunk.embedding.clone());
        sqlx::query(
            "INSERT INTO session_log_chunks (id, chunk_index, content, embedding, session_log_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(chunk.id)
        .bind(chunk.chunk_index)
        .bind(&chunk.content)
        .bind(embedding)
        .bind(chunk.session_log_id)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Upsert a parent session log row.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn session_log_upsert(pool: &PgPool, log: &SessionLog) -> Result<Uuid, sqlx::Error> {
    let embedding = Vector::from(log.embedding.clone());
    let row = sqlx::query(
        "INSERT INTO session_logs (id, content, created_at, cwd, embedding, project, session_id, summary)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (session_id) DO UPDATE SET
            content = EXCLUDED.content,
            cwd = EXCLUDED.cwd,
            embedding = EXCLUDED.embedding,
            project = EXCLUDED.project,
            summary = EXCLUDED.summary
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
    .fetch_one(pool)
    .await?;
    row.try_get("id")
}

/// Mark a normalized session as finalized.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn update_session_finalized(
    pool: &PgPool,
    id: Uuid,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE sessions
         SET updated_at = $2, ended_at = COALESCE(ended_at, $2)
         WHERE id = $1",
    )
    .bind(id)
    .bind(updated_at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
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

pub struct HybridSearchParams<'a> {
    pub category: Option<&'a Category>,
    pub limit: i64,
    pub min_similarity: f64,
    pub project: &'a str,
    pub query: &'a str,
    pub tags: Option<&'a [String]>,
}

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
    let rows = match params.category {
        Some(cat) => {
            sqlx::query(
                "WITH vector_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
                    FROM memories
                    WHERE project = $2 AND category = $3
                      AND 1 - (embedding <=> $1) >= $5
                      AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
                    LIMIT $4
                ),
                fts_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC) AS rank_f
                    FROM memories
                    WHERE project = $2 AND category = $3
                      AND fts @@ plainto_tsquery('english', $6)
                      AND ($8::TEXT[] IS NULL OR tags @> $8::TEXT[])
                    LIMIT $4
                ),
                combined AS (
                    SELECT COALESCE(v.id, f.id) AS id,
                           (COALESCE(1.0 / (60 + v.rank_v), 0) + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
                    FROM vector_results v
                    FULL OUTER JOIN fts_results f ON v.id = f.id
                )
                SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary, m.tags, m.updated_at,
                       c.rrf_score AS similarity
                FROM combined c
                JOIN memories m ON m.id = c.id
                ORDER BY c.rrf_score DESC
                LIMIT $7",
            )
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
        }
        None => {
            sqlx::query(
                "WITH vector_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
                    FROM memories
                    WHERE project = $2
                      AND 1 - (embedding <=> $1) >= $4
                      AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
                    LIMIT $3
                ),
                fts_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC) AS rank_f
                    FROM memories
                    WHERE project = $2
                      AND fts @@ plainto_tsquery('english', $5)
                      AND ($7::TEXT[] IS NULL OR tags @> $7::TEXT[])
                    LIMIT $3
                ),
                combined AS (
                    SELECT COALESCE(v.id, f.id) AS id,
                           (COALESCE(1.0 / (60 + v.rank_v), 0) + COALESCE(1.0 / (60 + f.rank_f), 0))::FLOAT8 AS rrf_score
                    FROM vector_results v
                    FULL OUTER JOIN fts_results f ON v.id = f.id
                )
                SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary, m.tags, m.updated_at,
                       c.rrf_score AS similarity
                FROM combined c
                JOIN memories m ON m.id = c.id
                ORDER BY c.rrf_score DESC
                LIMIT $6",
            )
            .bind(&query_vec)
            .bind(params.project)
            .bind(fetch_limit)
            .bind(params.min_similarity)
            .bind(params.query)
            .bind(params.limit)
            .bind(&tag_arr)
            .fetch_all(pool)
            .await?
        }
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

    use crate::model::{Category, EdgeOrigin, EdgeRelation, Memory, MemoryEdge};

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
