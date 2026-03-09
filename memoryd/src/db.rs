use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::model::{
    Category, Memory, MemorySummary, Session, SessionLog, SessionLogChunk, SessionLogSummary,
    SessionMessage, SessionMessageSummary, SessionSummary,
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

/// List plans awaiting review.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn list_plan_review_queue(
    pool: &PgPool,
    project: &str,
    limit: i64,
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, category, content, created_at, project, summary, tags, updated_at
         FROM memories
         WHERE project = $1
           AND category = $2
           AND tags @> ARRAY['review-needed']::TEXT[]
         ORDER BY updated_at DESC
         LIMIT $3",
    )
    .bind(project)
    .bind(Category::Plan)
    .bind(limit)
    .fetch_all(pool)
    .await?;
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
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let rows = if include_general && project != crate::app::GENERAL_RULE_PROJECT {
        sqlx::query(
            "SELECT id, category, content, created_at, project, summary, tags, updated_at
             FROM memories
             WHERE category = $1
               AND (project = $2 OR project = $3)
             ORDER BY CASE WHEN project = $3 THEN 0 ELSE 1 END, updated_at DESC",
        )
        .bind(Category::Rule)
        .bind(project)
        .bind(crate::app::GENERAL_RULE_PROJECT)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT id, category, content, created_at, project, summary, tags, updated_at
             FROM memories
             WHERE category = $1 AND project = $2
             ORDER BY updated_at DESC",
        )
        .bind(Category::Rule)
        .bind(project)
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
) -> Result<Vec<MemorySummary>, sqlx::Error> {
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1 AND category = $2
                 ORDER BY updated_at DESC
                 LIMIT $3 OFFSET $4",
            )
            .bind(project)
            .bind(cat)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, category, content, created_at, project, summary, tags, updated_at
                 FROM memories
                 WHERE project = $1
                 ORDER BY updated_at DESC
                 LIMIT $2 OFFSET $3",
            )
            .bind(project)
            .bind(limit)
            .bind(offset)
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
    let rows = sqlx::query("SELECT DISTINCT project FROM memories ORDER BY project")
        .fetch_all(pool)
        .await?;
    rows.iter().map(|row| row.try_get("project")).collect()
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

/// Run hybrid semantic plus full-text memory search.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn hybrid_search(
    pool: &PgPool,
    embedding: Vec<f32>,
    query: &str,
    project: &str,
    category: Option<&Category>,
    limit: i64,
    min_similarity: f64,
) -> Result<Vec<(MemorySummary, f64)>, sqlx::Error> {
    let query_vec = Vector::from(embedding);
    let fetch_limit = limit * 3;
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "WITH vector_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
                    FROM memories
                    WHERE project = $2 AND category = $3
                      AND 1 - (embedding <=> $1) >= $5
                    LIMIT $4
                ),
                fts_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $6)) DESC) AS rank_f
                    FROM memories
                    WHERE project = $2 AND category = $3
                      AND fts @@ plainto_tsquery('english', $6)
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
            .bind(project)
            .bind(cat)
            .bind(fetch_limit)
            .bind(min_similarity)
            .bind(query)
            .bind(limit)
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
                    LIMIT $3
                ),
                fts_results AS (
                    SELECT id, ROW_NUMBER() OVER (ORDER BY ts_rank_cd(fts, plainto_tsquery('english', $5)) DESC) AS rank_f
                    FROM memories
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
                SELECT m.id, m.category, m.content, m.created_at, m.project, m.summary, m.tags, m.updated_at,
                       c.rrf_score AS similarity
                FROM combined c
                JOIN memories m ON m.id = c.id
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
