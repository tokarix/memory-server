use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::model::{Category, Memory, MemorySummary, SessionLog, SessionLogSummary};

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

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

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

pub async fn list_projects(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT DISTINCT project FROM memories ORDER BY project")
        .fetch_all(pool)
        .await?;
    rows.iter().map(|row| row.try_get("project")).collect()
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

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
        "WITH vector_results AS (
            SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $1) AS rank_v
            FROM session_logs
            WHERE project = $2
              AND 1 - (embedding <=> $1) >= $4
            LIMIT $3
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

pub async fn session_log_upsert(pool: &PgPool, log: &SessionLog) -> Result<(), sqlx::Error> {
    let embedding = Vector::from(log.embedding.clone());
    sqlx::query(
        "INSERT INTO session_logs (id, content, created_at, cwd, embedding, project, session_id, summary)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (session_id) DO UPDATE SET
            content = EXCLUDED.content,
            cwd = EXCLUDED.cwd,
            embedding = EXCLUDED.embedding,
            project = EXCLUDED.project,
            summary = EXCLUDED.summary",
    )
    .bind(log.id)
    .bind(&log.content)
    .bind(log.created_at)
    .bind(&log.cwd)
    .bind(embedding)
    .bind(&log.project)
    .bind(&log.session_id)
    .bind(&log.summary)
    .execute(pool)
    .await?;
    Ok(())
}

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
