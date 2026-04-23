use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::embed;
use crate::error::Error;
use crate::model::{self, Category, MemoryEdgeSummary};
use crate::protocol::{
    AppendSessionMessageRequest, BootstrapPayload, CreateSessionRequest, FinalizeSessionRequest,
    ListMemoriesRequest, RuleList, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};
use crate::{db, edges, expand, rerank, transcript};

const CHUNK_OVERLAP: usize = 200;
const CHUNK_SIZE: usize = 4000;
const DEFAULT_MIN_SIMILARITY: f64 = 0.5;
pub const GENERAL_RULE_PROJECT: &str = "general";

#[derive(Clone)]
pub struct MemoryApp {
    embed_client: Arc<embed::Client>,
    expand_model: String,
    expand_num_ctx: u32,
    http: reqwest::Client,
    ollama_url: String,
    pool: PgPool,
    rerank_model: String,
    rerank_num_ctx: u32,
}

impl MemoryApp {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        pool: PgPool,
        embed_client: Arc<embed::Client>,
        expand_model: String,
        expand_num_ctx: u32,
        http: reqwest::Client,
        ollama_url: String,
        rerank_model: String,
        rerank_num_ctx: u32,
    ) -> Self {
        Self {
            embed_client,
            expand_model,
            expand_num_ctx,
            http,
            ollama_url,
            pool,
            rerank_model,
            rerank_num_ctx,
        }
    }

    #[must_use]
    pub fn version(&self) -> String {
        format!("{}-{}", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"))
    }

    /// Delete a memory by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn delete_memory(&self, id: Uuid) -> Result<bool, Error> {
        db::delete(&self.pool, id).await.map_err(Error::from)
    }

    /// Fetch one memory by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn get_memory(&self, id: Uuid) -> Result<Option<model::MemorySummary>, Error> {
        db::get(&self.pool, id).await.map_err(Error::from)
    }

    /// Fetch one finalized session log by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn get_session_log(
        &self,
        id: Uuid,
    ) -> Result<Option<model::SessionLogSummary>, Error> {
        db::get_session_log(&self.pool, id)
            .await
            .map_err(Error::from)
    }

    /// List memories for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
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
            request.tags.as_deref(),
        )
        .await
        .map_err(Error::from)
    }

    /// List all known projects across memories and sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_projects(&self) -> Result<Vec<String>, Error> {
        db::list_projects(&self.pool).await.map_err(Error::from)
    }

    /// List neighbor memories reachable via graph edges.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_neighbors(
        &self,
        memory_id: Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<(MemoryEdgeSummary, model::MemorySummary)>, Error> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        db::list_neighbors(&self.pool, memory_id, limit)
            .await
            .map_err(Error::from)
    }

    /// List finalized session logs for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_session_logs(
        &self,
        project: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<model::SessionLogSummary>, Error> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let offset = offset.unwrap_or(0).max(0);
        db::list_session_logs(&self.pool, project, limit, offset)
            .await
            .map_err(Error::from)
    }

    /// List normalized sessions for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_sessions(
        &self,
        project: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<model::SessionSummary>, Error> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let offset = offset.unwrap_or(0).max(0);
        db::list_sessions(&self.pool, project, limit, offset)
            .await
            .map_err(Error::from)
    }

    /// Load core memories for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn recall_project(&self, project: &str) -> Result<Vec<model::MemorySummary>, Error> {
        db::list_core(&self.pool, project)
            .await
            .map_err(Error::from)
    }

    /// Create or upsert a normalized session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<model::SessionSummary, Error> {
        let cwd = request.cwd.unwrap_or_default();
        let project = request.project.unwrap_or_else(|| project_from_cwd(&cwd));
        let now = Utc::now();
        let session = model::Session {
            agent: request.agent.unwrap_or_default(),
            created_at: now,
            cwd,
            ended_at: None,
            external_session_id: request.external_session_id,
            id: Uuid::new_v4(),
            project,
            updated_at: now,
        };
        db::create_session(&self.pool, &session)
            .await
            .map_err(Error::from)
    }

    /// Append one message to a normalized session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn append_session_message(
        &self,
        request: AppendSessionMessageRequest,
    ) -> Result<model::SessionMessageSummary, Error> {
        let message = model::SessionMessage {
            agent: request.agent.unwrap_or_default(),
            content: request.content,
            created_at: Utc::now(),
            id: Uuid::new_v4(),
            kind: request.kind.unwrap_or_else(|| "message".to_owned()),
            metadata: request.metadata,
            role: request.role,
            session_id: request.session_id,
        };
        db::append_session_message(&self.pool, &message)
            .await
            .map_err(Error::from)
    }

    /// Finalize a normalized session into searchable log chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if loading, embedding, or persistence fails.
    pub async fn finalize_session(
        &self,
        request: FinalizeSessionRequest,
    ) -> Result<Option<usize>, Error> {
        let Some(session) = db::get_session(&self.pool, request.session_id)
            .await
            .map_err(Error::from)?
        else {
            return Ok(None);
        };
        let messages = db::list_session_messages(&self.pool, request.session_id)
            .await
            .map_err(Error::from)?;
        let finalized = self
            .materialize_session_log(&session, &messages, request.summary.as_deref())
            .await?;
        Ok(Some(finalized))
    }

    /// Fetch one normalized session by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn get_session(&self, id: Uuid) -> Result<Option<model::SessionSummary>, Error> {
        db::get_session(&self.pool, id).await.map_err(Error::from)
    }

    /// Fetch all messages for one normalized session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_session_messages(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<model::SessionMessageSummary>, Error> {
        db::list_session_messages(&self.pool, session_id)
            .await
            .map_err(Error::from)
    }

    /// Load durable rules for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_rules(
        &self,
        project: &str,
        include_general: bool,
        shadow_general: bool,
        tags: Option<&[String]>,
    ) -> Result<RuleList, Error> {
        let rules = db::list_rules(&self.pool, project, include_general, shadow_general, tags)
            .await
            .map_err(Error::from)?;
        let (general_rules, project_rules): (Vec<_>, Vec<_>) = rules
            .into_iter()
            .partition(|memory| memory.project == GENERAL_RULE_PROJECT);
        Ok(RuleList {
            general_rules,
            project_rules,
        })
    }

    /// Load effective rules and optional recall memories for a project.
    ///
    /// # Errors
    ///
    /// Returns an error if rule or recall loading fails.
    pub async fn bootstrap_project(
        &self,
        project: &str,
        include_general: bool,
        include_recall: bool,
    ) -> Result<BootstrapPayload, Error> {
        let rules = self
            .list_rules(project, include_general, false, None)
            .await?;
        let recall_memories = if include_recall {
            self.recall_project(project)
                .await?
                .into_iter()
                .filter(|memory| {
                    memory.category != Category::Rule
                        && memory.category != Category::Plan
                        && !memory.tags.iter().any(|t| t == "review")
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(BootstrapPayload {
            general_rules: rules.general_rules,
            project: project.to_owned(),
            project_rules: rules.project_rules,
            recall_memories,
        })
    }

    /// Search memories and fall back to session logs when needed.
    ///
    /// # Errors
    ///
    /// Returns an error if expansion, embedding, reranking, or database retrieval fails.
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
        let use_query_expansion = request.expand_query.unwrap_or(false);

        let queries = if use_query_expansion {
            expand::expand_query(
                &self.http,
                &self.ollama_url,
                &self.expand_model,
                self.expand_num_ctx,
                &request.query,
            )
            .await
        } else {
            vec![request.query.clone()]
        };

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
                db::HybridSearchParams {
                    category: request.category.as_ref(),
                    limit: inner_limit,
                    min_similarity,
                    project: &request.project,
                    query,
                    tags: request.tags.as_deref(),
                },
            )
            .await
            .map_err(Error::from)?;
            variant_results.push(results);
        }

        let mut results = outer_rrf(&variant_results, limit);

        // Graph expansion: insert between outer RRF and rerank.
        let policy = edges::ExpansionPolicy {
            cross_project: request.cross_project.unwrap_or(false),
            graph_hops: request.graph_hops.unwrap_or(1),
            include_general: request.include_general.unwrap_or(false),
            project_allowlist: request.project_allowlist,
            source_project: request.project.clone(),
        };
        match edges::graph_expand(&self.pool, &results, &policy).await {
            Ok(expanded) => {
                if !expanded.is_empty() {
                    tracing::debug!(count = expanded.len(), "graph expansion added neighbors");
                    results.extend(expanded);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "graph expansion failed, proceeding with seeds only");
            }
        }

        let mut results = if request.rerank.unwrap_or(false) {
            rerank::rerank(
                &self.http,
                &self.ollama_url,
                &self.rerank_model,
                self.rerank_num_ctx,
                &request.query,
                results,
            )
            .await
        } else {
            results
        };

        // Enforce the requested limit after graph expansion + rerank.
        let final_limit = usize::try_from(limit).unwrap_or(usize::MAX);
        results.truncate(final_limit);

        if !results.is_empty() {
            return Ok(SearchOutcome::Memories(results));
        }
        // Session logs do not support structural tagging. If the caller explicitly
        // requested tag-filtered results, suppress the untagged fallback entirely.
        if request.tags.is_some() {
            return Ok(SearchOutcome::Memories(vec![]));
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

    /// Store a new memory and embed it.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or persistence fails.
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
        let summary = model::MemorySummary {
            id: memory.id,
            category: memory.category,
            content: memory.content,
            created_at: memory.created_at,
            project: memory.project,
            summary: memory.summary,
            tags: memory.tags,
            updated_at: memory.updated_at,
        };

        if let Err(e) = edges::build_write_time_edges(&self.pool, &summary).await {
            tracing::warn!(id = %summary.id, error = %e, "failed to build write-time edges");
        }

        Ok(summary)
    }

    /// Update a memory and re-embed when needed.
    ///
    /// # Errors
    ///
    /// Returns an error if loading, embedding, or persistence fails.
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

        let result = db::get(&self.pool, request.id).await.map_err(Error::from)?;
        if let Some(ref memory) = result
            && let Err(e) = edges::build_write_time_edges(&self.pool, memory).await
        {
            tracing::warn!(id = %request.id, error = %e, "failed to rebuild edges after update");
        }
        Ok(result)
    }

    /// Store a full transcript as a searchable session log.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or persistence fails.
    pub async fn store_session_log(&self, request: StoreSessionLogRequest) -> Result<usize, Error> {
        let cwd = request.cwd.unwrap_or_default();
        let project = request.project.unwrap_or_else(|| project_from_cwd(&cwd));
        self.materialize_raw_session_log(
            &request.session_id,
            &cwd,
            &project,
            &request.content,
            &request.summary,
        )
        .await
    }

    /// List memories that are waiting for review.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn list_review_queue(
        &self,
        project: &str,
        category: Option<&Category>,
        limit: i64,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        db::list_review_queue(&self.pool, project, category, limit)
            .await
            .map_err(Error::from)
    }

    /// Submit a review and retag the reviewed memory.
    ///
    /// # Errors
    ///
    /// Returns an error if loading or updating the memory fails.
    pub async fn submit_review(
        &self,
        memory_id: Uuid,
        project: Option<String>,
        reviewer: String,
        verdict: String,
        notes: String,
    ) -> Result<Option<model::MemorySummary>, Error> {
        let Some(original) = db::get(&self.pool, memory_id).await.map_err(Error::from)? else {
            return Ok(None);
        };

        let normalized_verdict = verdict.trim().to_lowercase();
        let review_project = project.unwrap_or_else(|| original.project.clone());
        let category_label = capitalize_category(&original.category);
        let review_summary = format!(
            "{category_label} review by {reviewer}: {}",
            original.summary
        );
        let review_content = notes;
        let mut updated_tags: Vec<String> = original
            .tags
            .iter()
            .filter(|tag| tag.as_str() != "review-needed")
            .cloned()
            .collect();
        updated_tags.push("reviewed".to_owned());
        updated_tags.push(format!("reviewed-by:{reviewer}"));
        updated_tags.push(format!("review-verdict:{normalized_verdict}"));
        updated_tags.sort();
        updated_tags.dedup();

        let review = self
            .store_memory(StoreMemoryRequest {
                category: Category::Decision,
                content: review_content,
                project: review_project,
                summary: review_summary,
                tags: Some(vec![
                    "review".to_owned(),
                    format!("reviewed-item:{memory_id}"),
                    format!("reviewer:{reviewer}"),
                    format!("verdict:{normalized_verdict}"),
                ]),
            })
            .await?;

        self.update_memory(UpdateMemoryRequest {
            content: None,
            id: memory_id,
            summary: None,
            tags: Some(updated_tags),
        })
        .await?;

        Ok(Some(review))
    }
}

impl MemoryApp {
    async fn materialize_raw_session_log(
        &self,
        session_id: &str,
        cwd: &str,
        project: &str,
        content: &str,
        summary: &str,
    ) -> Result<usize, Error> {
        let embedding = self.embed_client.embed(summary, "").await?;
        let text_chunks = transcript::chunk_text(content, CHUNK_SIZE, CHUNK_OVERLAP);
        let log = model::SessionLog {
            id: Uuid::new_v4(),
            content: content.to_owned(),
            created_at: Utc::now(),
            cwd: cwd.to_owned(),
            embedding,
            project: project.to_owned(),
            session_id: session_id.to_owned(),
            summary: summary.to_owned(),
        };
        let stored_id = db::session_log_upsert(&self.pool, &log)
            .await
            .map_err(Error::from)?;
        self.store_session_chunks(stored_id, &text_chunks).await
    }

    async fn materialize_session_log(
        &self,
        session: &model::SessionSummary,
        messages: &[model::SessionMessageSummary],
        summary_override: Option<&str>,
    ) -> Result<usize, Error> {
        let (content, summary) = aggregate_session_messages(messages, summary_override);
        let chunk_count = self
            .materialize_raw_session_log(
                &session.external_session_id,
                &session.cwd,
                &session.project,
                &content,
                &summary,
            )
            .await?;
        db::update_session_finalized(&self.pool, session.id, Utc::now())
            .await
            .map_err(Error::from)?;
        Ok(chunk_count)
    }

    async fn store_session_chunks(
        &self,
        stored_id: Uuid,
        text_chunks: &[String],
    ) -> Result<usize, Error> {
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

fn aggregate_session_messages(
    messages: &[model::SessionMessageSummary],
    summary_override: Option<&str>,
) -> (String, String) {
    let mut content = String::new();
    let mut prompts = Vec::new();

    for message in messages {
        let label = format_session_label(message);
        if message.role == "user" {
            prompts.push(message.content.clone());
        }
        content.push_str(&label);
        content.push_str(": ");
        content.push_str(&message.content);
        content.push('\n');
    }

    let mut summary = summary_override.map_or_else(|| prompts.join(" | "), str::to_owned);
    truncate_to_char_boundary(&mut summary, 2_000);
    truncate_to_char_boundary(&mut content, 50_000);
    (content, summary)
}

fn format_session_label(message: &model::SessionMessageSummary) -> String {
    let base = match message.role.as_str() {
        "assistant" => "Assistant",
        "system" => "System",
        "tool" => "Tool",
        _ => "User",
    };
    if message.agent.is_empty() {
        base.to_owned()
    } else {
        format!("{base} ({})", message.agent)
    }
}

fn capitalize_category(category: &Category) -> String {
    let s = category.to_string();
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => s,
    }
}

fn project_from_cwd(cwd: &str) -> String {
    cwd.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn truncate_to_char_boundary(s: &mut String, max_len: usize) {
    if s.len() <= max_len {
        return;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, Memory};
    use axum::{Json, Router, routing::post};
    use chrono::Utc;
    use sqlx::PgPool;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use uuid::Uuid;

    async fn mock_embed() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "embeddings": [vec![0.0f32; 1024]]
        }))
    }

    async fn mock_show() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 8192
            }
        }))
    }

    async fn spawn_mock_server() -> String {
        let app = Router::new()
            .route("/api/embed", post(mock_embed))
            .route("/api/show", post(mock_show));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[ignore = "requires DATABASE_URL"]
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn submit_review_stores_notes_as_content_and_retags(pool: PgPool) {
        let mock_url = spawn_mock_server().await;
        let embed_client = Arc::new(crate::embed::Client::new(
            mock_url.clone(),
            "test-model".to_owned(),
            None,
            None,
        ));
        let app = MemoryApp::new(
            pool.clone(),
            embed_client,
            "test-model".to_owned(),
            1024,
            reqwest::Client::new(),
            mock_url,
            "test-model".to_owned(),
            1024,
        );

        let memory_id = Uuid::new_v4();
        let mem = Memory {
            id: memory_id,
            category: Category::Plan,
            content: "old plan content".to_owned(),
            created_at: Utc::now(),
            embedding: vec![0.0; 1024],
            project: "test_proj".to_owned(),
            summary: "test plan summary".to_owned(),
            tags: vec!["review-needed".to_owned()],
            updated_at: Utc::now(),
        };
        crate::db::insert(&pool, &mem).await.unwrap();

        let review = app
            .submit_review(
                memory_id,
                None,
                "test_reviewer".to_owned(),
                "CHANGES-REQUESTED".to_owned(),
                "These are my review notes.".to_owned(),
            )
            .await
            .unwrap()
            .expect("Review created");

        assert_eq!(review.category, Category::Decision);
        assert_eq!(review.content, "These are my review notes.");
        assert_eq!(
            review.summary,
            "Plan review by test_reviewer: test plan summary"
        );
        assert!(review.tags.contains(&"reviewer:test_reviewer".to_owned()));
        assert!(
            review
                .tags
                .contains(&"verdict:changes-requested".to_owned())
        );
        assert!(review.tags.contains(&format!("reviewed-item:{memory_id}")));

        let updated_original = app.get_memory(memory_id).await.unwrap().unwrap();
        assert!(!updated_original.tags.contains(&"review-needed".to_owned()));
        assert!(updated_original.tags.contains(&"reviewed".to_owned()));
        assert!(
            updated_original
                .tags
                .contains(&"review-verdict:changes-requested".to_owned())
        );
    }
}
