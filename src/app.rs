use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::embed;
use crate::error::Error;
use crate::model::{self, Category};
use crate::{db, expand, rerank, transcript};

const CHUNK_OVERLAP: usize = 200;
const CHUNK_SIZE: usize = 4000;
const DEFAULT_MIN_SIMILARITY: f64 = 0.5;

#[derive(Clone)]
pub struct MemoryApp {
    embed_client: Arc<embed::Client>,
    expand_model: String,
    generate_num_ctx: u32,
    http: reqwest::Client,
    ollama_url: String,
    pool: PgPool,
    rerank_model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListMemoriesRequest {
    pub category: Option<Category>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub project: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchMemoriesRequest {
    pub category: Option<Category>,
    pub limit: Option<i64>,
    pub min_similarity: Option<f64>,
    pub project: String,
    pub query: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreMemoryRequest {
    pub category: Category,
    pub content: String,
    pub project: String,
    pub summary: String,
    pub tags: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateMemoryRequest {
    pub content: Option<String>,
    pub id: Uuid,
    pub summary: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreSessionLogRequest {
    pub content: String,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub session_id: String,
    pub summary: String,
}

pub enum SearchOutcome {
    Memories(Vec<(model::MemorySummary, f64)>),
    SessionLogs(Vec<(model::SessionLogSummary, f64)>),
    Empty,
}

impl MemoryApp {
    pub fn new(
        pool: PgPool,
        embed_client: Arc<embed::Client>,
        expand_model: String,
        generate_num_ctx: u32,
        http: reqwest::Client,
        ollama_url: String,
        rerank_model: String,
    ) -> Self {
        Self {
            embed_client,
            expand_model,
            generate_num_ctx,
            http,
            ollama_url,
            pool,
            rerank_model,
        }
    }

    #[must_use]
    pub fn version(&self) -> String {
        format!("{}-{}", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"))
    }

    pub async fn delete_memory(&self, id: Uuid) -> Result<bool, Error> {
        db::delete(&self.pool, id).await.map_err(Error::from)
    }

    pub async fn get_memory(&self, id: Uuid) -> Result<Option<model::MemorySummary>, Error> {
        db::get(&self.pool, id).await.map_err(Error::from)
    }

    pub async fn list_memories(
        &self,
        request: ListMemoriesRequest,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        let limit = request.limit.unwrap_or(20).clamp(1, 100);
        let offset = request.offset.unwrap_or(0).max(0);
        db::list(
            &self.pool,
            &request.project,
            request.category.as_ref(),
            limit,
            offset,
        )
        .await
        .map_err(Error::from)
    }

    pub async fn recall_project(&self, project: &str) -> Result<Vec<model::MemorySummary>, Error> {
        db::list_core(&self.pool, project)
            .await
            .map_err(Error::from)
    }

    pub async fn search_memories(
        &self,
        request: SearchMemoriesRequest,
    ) -> Result<SearchOutcome, Error> {
        let limit = request.limit.unwrap_or(5).clamp(1, 100);
        let min_similarity = request
            .min_similarity
            .unwrap_or(DEFAULT_MIN_SIMILARITY)
            .clamp(0.0, 1.0);
        let inner_limit = limit * 2;

        let queries = expand::expand_query(
            &self.http,
            &self.ollama_url,
            &self.expand_model,
            self.generate_num_ctx,
            &request.query,
        )
        .await;

        let mut variant_results = Vec::with_capacity(queries.len());
        let mut first_embedding = None;
        for query in &queries {
            let embedding = self.embed_client.embed(query, "").await?;
            if first_embedding.is_none() {
                first_embedding = Some(embedding.clone());
            }
            let results = db::hybrid_search(
                &self.pool,
                embedding,
                query,
                &request.project,
                request.category.as_ref(),
                inner_limit,
                min_similarity,
            )
            .await
            .map_err(Error::from)?;
            variant_results.push(results);
        }

        let results = outer_rrf(&variant_results, limit);
        let results = rerank::rerank(
            &self.http,
            &self.ollama_url,
            &self.rerank_model,
            self.generate_num_ctx,
            &request.query,
            results,
        )
        .await;

        if !results.is_empty() {
            return Ok(SearchOutcome::Memories(results));
        }

        if let Some(embedding) = first_embedding {
            let session_results = db::session_log_search(
                &self.pool,
                embedding,
                &request.query,
                &request.project,
                limit,
                min_similarity,
            )
            .await
            .map_err(Error::from)?;
            if !session_results.is_empty() {
                return Ok(SearchOutcome::SessionLogs(session_results));
            }
        }

        Ok(SearchOutcome::Empty)
    }

    pub async fn store_memory(
        &self,
        request: StoreMemoryRequest,
    ) -> Result<model::MemorySummary, Error> {
        let embedding = self
            .embed_client
            .embed(&request.summary, &request.content)
            .await?;
        let now = Utc::now();
        let memory = model::Memory {
            id: Uuid::new_v4(),
            category: request.category,
            content: request.content,
            created_at: now,
            embedding,
            project: request.project,
            summary: request.summary,
            tags: request.tags.unwrap_or_default(),
            updated_at: now,
        };
        db::insert(&self.pool, &memory).await.map_err(Error::from)?;
        Ok(model::MemorySummary {
            id: memory.id,
            category: memory.category,
            content: memory.content,
            created_at: memory.created_at,
            project: memory.project,
            summary: memory.summary,
            tags: memory.tags,
            updated_at: memory.updated_at,
        })
    }

    pub async fn update_memory(
        &self,
        request: UpdateMemoryRequest,
    ) -> Result<Option<model::MemorySummary>, Error> {
        let embedding = if request.summary.is_some() || request.content.is_some() {
            let current = db::get(&self.pool, request.id)
                .await
                .map_err(Error::from)?
                .ok_or_else(|| Error::NotFound(format!("memory {}", request.id)))?;
            let summary = request.summary.as_deref().unwrap_or(&current.summary);
            let content = request.content.as_deref().unwrap_or(&current.content);
            Some(self.embed_client.embed(summary, content).await?)
        } else {
            None
        };

        let updated = db::update(
            &self.pool,
            request.id,
            request.content.as_deref(),
            embedding,
            request.summary.as_deref(),
            request.tags.as_deref(),
        )
        .await
        .map_err(Error::from)?;

        if !updated {
            return Ok(None);
        }

        db::get(&self.pool, request.id).await.map_err(Error::from)
    }

    pub async fn store_session_log(&self, request: StoreSessionLogRequest) -> Result<usize, Error> {
        let cwd = request.cwd.unwrap_or_default();
        let project = request.project.unwrap_or_else(|| {
            cwd.rsplit('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or("")
                .to_owned()
        });
        let embedding = self.embed_client.embed(&request.summary, "").await?;

        let text_chunks = transcript::chunk_text(&request.content, CHUNK_SIZE, CHUNK_OVERLAP);
        let log = model::SessionLog {
            id: Uuid::new_v4(),
            content: request.content,
            created_at: Utc::now(),
            cwd,
            embedding,
            project,
            session_id: request.session_id,
            summary: request.summary,
        };
        let stored_id = db::session_log_upsert(&self.pool, &log)
            .await
            .map_err(Error::from)?;

        let mut chunks = Vec::with_capacity(text_chunks.len());
        for (index, text) in text_chunks.iter().enumerate() {
            let chunk_embedding = self.embed_client.embed(text, "").await?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            chunks.push(model::SessionLogChunk {
                chunk_index: index as i32,
                content: text.clone(),
                embedding: chunk_embedding,
                id: Uuid::new_v4(),
                session_log_id: stored_id,
            });
        }

        db::session_log_chunks_replace(&self.pool, stored_id, &chunks)
            .await
            .map_err(Error::from)?;
        Ok(chunks.len())
    }
}

pub(crate) fn outer_rrf(
    variant_results: &[Vec<(model::MemorySummary, f64)>],
    limit: i64,
) -> Vec<(model::MemorySummary, f64)> {
    let mut rrf_scores: HashMap<Uuid, f64> = HashMap::new();
    let mut memories: HashMap<Uuid, &model::MemorySummary> = HashMap::new();

    for (variant_idx, results) in variant_results.iter().enumerate() {
        let weight = if variant_idx == 0 { 2.0 } else { 1.0 };
        for (rank_idx, (memory, _)) in results.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let contribution = weight / (60.0 + (rank_idx + 1) as f64);
            *rrf_scores.entry(memory.id).or_default() += contribution;
            memories.entry(memory.id).or_insert(memory);
        }
    }

    let mut ranked: Vec<(Uuid, f64)> = rrf_scores.into_iter().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n = usize::try_from(limit).unwrap_or(usize::MAX);
    ranked.truncate(n);

    ranked
        .into_iter()
        .filter_map(|(id, score)| memories.get(&id).map(|memory| ((*memory).clone(), score)))
        .collect()
}
