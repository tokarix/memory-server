use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::model::{Category, Memory};

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
) -> Result<Vec<Memory>, sqlx::Error> {
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "SELECT id, category, content, created_at, embedding, project, summary, tags, updated_at
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
                "SELECT id, category, content, created_at, embedding, project, summary, tags, updated_at
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
    rows.iter().map(row_to_memory).collect()
}

pub async fn search(
    pool: &PgPool,
    embedding: Vec<f32>,
    project: &str,
    category: Option<&Category>,
    limit: i64,
) -> Result<Vec<(Memory, f64)>, sqlx::Error> {
    let query_vec = Vector::from(embedding);
    let rows = match category {
        Some(cat) => {
            sqlx::query(
                "SELECT id, category, content, created_at, embedding, project, summary, tags, updated_at,
                        1 - (embedding <=> $1) AS similarity
                 FROM memories
                 WHERE project = $2 AND category = $3
                 ORDER BY embedding <=> $1
                 LIMIT $4",
            )
            .bind(&query_vec)
            .bind(project)
            .bind(cat)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, category, content, created_at, embedding, project, summary, tags, updated_at,
                        1 - (embedding <=> $1) AS similarity
                 FROM memories
                 WHERE project = $2
                 ORDER BY embedding <=> $1
                 LIMIT $3",
            )
            .bind(&query_vec)
            .bind(project)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    rows.iter()
        .map(|row| {
            let memory = row_to_memory(row)?;
            let similarity: f64 = row.try_get("similarity")?;
            Ok((memory, similarity))
        })
        .collect()
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

fn row_to_memory(row: &sqlx::postgres::PgRow) -> Result<Memory, sqlx::Error> {
    let embedding: Vector = row.try_get("embedding")?;
    Ok(Memory {
        id: row.try_get("id")?,
        category: row.try_get("category")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        embedding: embedding.into(),
        project: row.try_get("project")?,
        summary: row.try_get("summary")?,
        tags: row.try_get("tags")?,
        updated_at: row.try_get("updated_at")?,
    })
}
