use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::embed;
use crate::error::Error;
use crate::model::{self, Category, MemoryEdgeSummary};
use crate::policy_resolution::{self, ResolutionRequest};
use crate::protocol::{
    AppendSessionMessageRequest, BootstrapPayload, CreateSessionRequest, FinalizeSessionRequest,
    ListMemoriesRequest, RuleList, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};
use crate::{db, edges, expand, policy, rerank, transcript, workflow};

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
        let deleted = db::delete(&self.pool, id).await.map_err(Error::from)?;
        if !deleted
            && let Some(memory) = db::get(&self.pool, id).await.map_err(Error::from)?
            && memory.policy.is_some()
        {
            return Err(immutable_revision_error(&memory));
        }
        Ok(deleted)
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
    pub async fn recall_project(
        &self,
        project: &str,
        include_workflow_artifacts: Option<bool>,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        db::list_core(
            &self.pool,
            project,
            include_workflow_artifacts.unwrap_or(false),
        )
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
            workflow_artifact: false,
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
            .map_err(Error::from)?
            .ok_or_else(|| Error::NotFound(format!("session {}", request.session_id)))
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
        self.materialize_session_log(&session, &messages, request.summary.as_deref())
            .await
            .map(Some)
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
        self.resolve_rules(ResolutionRequest {
            project: project.to_owned(),
            include_general,
            shadow_general,
            tags: tags.map(<[String]>::to_vec),
            context: memory_common::policy::ResolutionContext::default(),
        })
        .await
    }

    /// Resolve policies from a complete one-statement Rule snapshot.
    ///
    /// # Errors
    /// Returns a database or typed policy resolution error.
    pub async fn resolve_rules(&self, request: ResolutionRequest) -> Result<RuleList, Error> {
        let candidates = db::load_rule_candidates(&self.pool, &request.project)
            .await
            .map_err(Error::from)?;
        policy_resolution::resolve(candidates, &request)
    }

    /// Resolve and publish the complete mandatory pack for a trusted scope.
    ///
    /// # Errors
    /// Returns a typed policy or database error; no partial pack is published.
    pub async fn guardrails(
        &self,
        project: &str,
        context: memory_common::policy::ResolutionContext,
    ) -> Result<memory_common::guardrails::GuardrailPack, Error> {
        let rules = self
            .resolve_rules(ResolutionRequest {
                project: project.to_owned(),
                include_general: true,
                shadow_general: true,
                tags: None,
                context,
            })
            .await?;
        memory_common::guardrails::GuardrailPack::new(
            project.to_owned(),
            rules.context,
            rules.canonical.schema_version,
            rules.canonical.mandatory,
        )
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
        self.bootstrap_project_with_context(
            project,
            include_general,
            include_recall,
            memory_common::policy::ResolutionContext::default(),
        )
        .await
    }

    /// Resolve bootstrap policies using the supplied trusted execution context.
    ///
    /// # Errors
    /// Returns a policy error before loading recall if resolution fails.
    pub async fn bootstrap_project_with_context(
        &self,
        project: &str,
        include_general: bool,
        include_recall: bool,
        context: memory_common::policy::ResolutionContext,
    ) -> Result<BootstrapPayload, Error> {
        let rules = self
            .resolve_rules(ResolutionRequest {
                project: project.to_owned(),
                include_general,
                shadow_general: true,
                tags: None,
                context,
            })
            .await?;
        let recall_memories = if include_recall {
            self.recall_project(project, None)
                .await?
                .into_iter()
                .filter(|memory| {
                    memory.category != Category::Rule
                        && memory.category != Category::Plan
                        && !(memory.category == Category::Decision
                            && memory.tags.iter().any(|tag| tag == "review"))
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
            canonical: rules.canonical,
            context: rules.context,
            options: rules.options,
        })
    }

    /// Search memories and fall back to session logs when needed.
    ///
    /// # Errors
    ///
    /// Returns an error if expansion, embedding, reranking, or database retrieval fails.
    #[allow(clippy::too_many_lines)]
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
        let include_workflow_artifacts = request.include_workflow_artifacts.unwrap_or(false);

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
                    include_workflow_artifacts,
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
            include_workflow_artifacts,
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
                include_workflow_artifacts,
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
        if let Some(ref metadata) = request.policy {
            policy::validate_store(metadata, &request.category)?;
        }
        let embedding = self
            .embed_client
            .embed(&request.summary, &request.content)
            .await?;
        let now = Utc::now();
        let memory = model::Memory {
            id: Uuid::new_v4(),
            policy: None,
            category: request.category,
            content: request.content,
            created_at: now,
            embedding,
            project: request.project,
            summary: request.summary,
            tags: request.tags.unwrap_or_default(),
            updated_at: now,
        };
        if let Some(ref metadata) = request.policy {
            let summary = policy::publish_new(&self.pool, &memory, metadata).await?;
            if let Err(e) = edges::build_write_time_edges(&self.pool, &summary).await {
                tracing::warn!(id = %summary.id, error = %e, "failed to build policy graph edges");
            }
            return Ok(summary);
        }
        if matches!(memory.category, Category::Plan | Category::Decision) {
            db::insert_with_workflow_provenance(&self.pool, &memory)
                .await
                .map_err(Error::from)?;
        } else {
            db::insert(&self.pool, &memory).await.map_err(Error::from)?;
        }
        let summary = db::get(&self.pool, memory.id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| Error::Database(format!("stored memory {} disappeared", memory.id)))?;

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
        if let Some(ref metadata) = request.policy {
            if request.content.is_some() || request.summary.is_some() || request.tags.is_some() {
                return Err(Error::Policy {
                    code: "policy_invalid_adoption".to_owned(),
                    message: "adoption must contain only id, policy, and expected_updated_at"
                        .to_owned(),
                    details: serde_json::json!({"id": request.id, "policy_key": metadata.policy_key}),
                    conflict: false,
                });
            }
            let token = request.expected_updated_at.as_deref().ok_or_else(|| Error::Policy {
                code: "policy_missing_precondition".to_owned(),
                message: "copy the exact updated_at header from memory_get".to_owned(),
                details: serde_json::json!({"id": request.id, "policy_key": metadata.policy_key}),
                conflict: false,
            })?;
            let expected = policy::parse_expected_updated_at(token, metadata)?;
            let summary = policy::adopt(&self.pool, request.id, metadata, expected).await?;
            if let Err(e) = edges::build_write_time_edges(&self.pool, &summary).await {
                tracing::warn!(id = %summary.id, error = %e, "failed to rebuild policy graph edges");
            }
            return Ok(Some(summary));
        }
        if request.expected_updated_at.is_some() {
            return Err(Error::Policy {
                code: "policy_invalid_precondition".to_owned(),
                message: "expected_updated_at is only valid with policy adoption".to_owned(),
                details: serde_json::json!({"id": request.id}),
                conflict: false,
            });
        }
        let current = db::get(&self.pool, request.id).await.map_err(Error::from)?;
        let Some(current) = current else {
            if request.summary.is_some() || request.content.is_some() {
                return Err(Error::NotFound(format!("memory {}", request.id)));
            }
            return Ok(None);
        };
        if request.content.is_none() && request.summary.is_none() && request.tags.is_none() {
            return Ok(Some(current));
        }
        if current.policy.is_some() {
            return Err(immutable_revision_error(&current));
        }
        let embedding = if request.summary.is_some() || request.content.is_some() {
            let summary = request.summary.as_deref().unwrap_or(&current.summary);
            let content = request.content.as_deref().unwrap_or(&current.content);
            Some(self.embed_client.embed(summary, content).await?)
        } else {
            None
        };

        let updated = if matches!(current.category, Category::Plan | Category::Decision)
            && request.tags.is_some()
        {
            db::update_with_workflow_provenance(
                &self.pool,
                request.id,
                request.content.as_deref(),
                embedding,
                request.summary.as_deref(),
                request.tags.as_deref(),
            )
            .await
        } else {
            db::update(
                &self.pool,
                request.id,
                request.content.as_deref(),
                embedding,
                request.summary.as_deref(),
                request.tags.as_deref(),
            )
            .await
        }
        .map_err(Error::from)?;

        if !updated {
            if let Some(memory) = db::get(&self.pool, request.id).await.map_err(Error::from)?
                && memory.policy.is_some()
            {
                return Err(immutable_revision_error(&memory));
            }
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
        if original.policy.is_some() {
            return Err(immutable_revision_error(&original));
        }

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
                policy: None,
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
            expected_updated_at: None,
            id: memory_id,
            policy: None,
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
            workflow_artifact: workflow::contains_task_token(content)
                || workflow::contains_task_token(summary),
        };
        let chunks = self.prepare_session_chunks(log.id, &text_chunks).await?;
        db::publish_session_log(&self.pool, &log, &chunks, None, None)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| {
                Error::Database("raw publisher unexpectedly returned no log ID".to_owned())
            })?;
        Ok(chunks.len())
    }

    async fn materialize_session_log(
        &self,
        session: &model::SessionSummary,
        messages: &[model::SessionMessageSummary],
        summary_override: Option<&str>,
    ) -> Result<usize, Error> {
        let (content, summary) = aggregate_session_messages(messages, summary_override);
        let embedding = self.embed_client.embed(&summary, "").await?;
        let text_chunks = transcript::chunk_text(&content, CHUNK_SIZE, CHUNK_OVERLAP);
        let log = model::SessionLog {
            id: Uuid::new_v4(),
            content: content.clone(),
            created_at: Utc::now(),
            cwd: session.cwd.clone(),
            embedding,
            project: session.project.clone(),
            session_id: session.external_session_id.clone(),
            summary: summary.clone(),
            workflow_artifact: workflow::contains_task_token(&content)
                || workflow::contains_task_token(&summary),
        };
        let chunks = self.prepare_session_chunks(log.id, &text_chunks).await?;
        db::publish_session_log(
            &self.pool,
            &log,
            &chunks,
            Some(session.id),
            Some(Utc::now()),
        )
        .await
        .map_err(Error::from)?
        .ok_or_else(|| Error::NotFound(format!("session {}", session.id)))?;
        Ok(chunks.len())
    }

    async fn prepare_session_chunks(
        &self,
        session_log_id: Uuid,
        text_chunks: &[String],
    ) -> Result<Vec<model::SessionLogChunk>, Error> {
        let mut chunks = Vec::with_capacity(text_chunks.len());
        for (index, text) in text_chunks.iter().enumerate() {
            let chunk_embedding = self.embed_client.embed(text, "").await?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            // Publication assigns the authoritative parent provenance to every
            // prepared chunk inside the correlation-locked transaction.
            chunks.push(model::SessionLogChunk {
                chunk_index: index as i32,
                content: text.clone(),
                embedding: chunk_embedding,
                id: Uuid::new_v4(),
                session_log_id,
                workflow_artifact: false,
            });
        }
        Ok(chunks)
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

fn immutable_revision_error(memory: &model::MemorySummary) -> Error {
    Error::Policy {
        code: "policy_immutable_revision".to_owned(),
        message: format!(
            "Rule {} is a classified policy revision; publish a successor instead",
            memory.id
        ),
        details: serde_json::json!({
            "id": memory.id,
            "project": memory.project,
            "policy": memory.policy,
        }),
        conflict: true,
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
    use std::sync::Arc;

    use axum::http::StatusCode;
    use axum::{Json, Router, routing::post};
    use chrono::Utc;
    use memory_common::policy::{DeliveryClass, PolicyState, PolicyWrite};
    use sqlx::PgPool;
    use tokio::net::TcpListener;
    use uuid::Uuid;

    use crate::model::{Category, Memory};

    use super::*;

    async fn mock_embed() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "embeddings": [vec![1.0f32; 1024]]
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

    fn policy_request(supersedes: Option<Uuid>, revision: i64) -> StoreMemoryRequest {
        StoreMemoryRequest {
            category: Category::Rule,
            content: format!("Policy revision {revision}"),
            policy: Some(PolicyWrite {
                policy_key: "build.storage".to_owned(),
                revision,
                delivery_class: DeliveryClass::Contextual,
                supersedes,
                selectors: memory_common::policy::PolicySelectors::default(),
                values: std::collections::BTreeMap::new(),
            }),
            project: "project-a".to_owned(),
            summary: format!("Revision {revision}"),
            tags: Some(vec!["policy".to_owned()]),
        }
    }

    fn app_with_mock(pool: PgPool, url: String) -> MemoryApp {
        MemoryApp::new(
            pool,
            Arc::new(crate::embed::Client::new(
                url.clone(),
                "test-model".to_owned(),
                None,
                None,
            )),
            "test-model".to_owned(),
            1024,
            reqwest::Client::new(),
            url,
            "test-model".to_owned(),
            1024,
        )
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn graph_failure_after_commit_keeps_published_revision(pool: PgPool) {
        let app = app_with_mock(pool.clone(), spawn_mock_server().await);
        sqlx::query("DROP TABLE memory_edges")
            .execute(&pool)
            .await
            .unwrap();
        let published = app.store_memory(policy_request(None, 1)).await.unwrap();
        assert_eq!(
            published.policy.as_ref().unwrap().state,
            PolicyState::Active
        );
        assert!(db::get(&pool, published.id).await.unwrap().is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn embedding_failure_does_not_supersede_current_head(pool: PgPool) {
        let working_app = app_with_mock(pool.clone(), spawn_mock_server().await);
        let head = working_app
            .store_memory(policy_request(None, 1))
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/api/embed", post(|| async { StatusCode::BAD_GATEWAY }))
                    .route("/api/show", post(mock_show)),
            )
            .await
            .unwrap();
        });
        let failing_app = app_with_mock(pool.clone(), format!("http://{addr}"));
        assert!(
            failing_app
                .store_memory(policy_request(Some(head.id), 2))
                .await
                .is_err()
        );
        assert_eq!(
            db::get(&pool, head.id)
                .await
                .unwrap()
                .unwrap()
                .policy
                .unwrap()
                .state,
            PolicyState::Active
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn real_http_rules_and_bootstrap_resolve_scoped_policies(pool: PgPool) {
        use std::collections::BTreeSet;

        use crate::api::{self, ApiState};

        fn scoped_memory(project: &str, id: u128) -> Memory {
            let now = Utc::now();
            Memory {
                id: Uuid::from_u128(id),
                policy: None,
                category: Category::Rule,
                content: format!("Policy {id}"),
                created_at: now,
                embedding: vec![0.0; 1024],
                project: project.to_owned(),
                summary: format!("Policy {id}"),
                tags: vec!["hidden".to_owned()],
                updated_at: now,
            }
        }

        let app = app_with_mock(pool.clone(), spawn_mock_server().await);
        for (id, key, profile, storage) in [
            (1, "storage.host", "workstation-host", "persistent-disk"),
            (
                2,
                "storage.ci",
                "woodpecker-container",
                "container-local-tmp",
            ),
        ] {
            let mut write = policy_request(None, 1).policy.unwrap();
            write.policy_key = key.to_owned();
            write.delivery_class = DeliveryClass::Mandatory;
            write.selectors.profile = Some(BTreeSet::from([profile.to_owned()]));
            write
                .values
                .insert("build.target".to_owned(), storage.to_owned());
            crate::policy::publish_new(&pool, &scoped_memory("general", id), &write)
                .await
                .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                api::router(ApiState {
                    app,
                    bearer_token: None,
                }),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        let url = |endpoint: &str, pairs: &[(&str, &str)]| {
            let mut url =
                reqwest::Url::parse(&format!("http://{addr}/api/v1/projects/app/{endpoint}"))
                    .unwrap();
            url.query_pairs_mut().extend_pairs(pairs.iter().copied());
            url
        };
        for (profile, expected_id) in [("workstation-host", 1), ("woodpecker-container", 2)] {
            for endpoint in ["rules", "bootstrap"] {
                let context = format!("{{\"profile\":\"{profile}\"}}");
                let response = client
                    .get(url(
                        endpoint,
                        &[
                            ("include_general", "false"),
                            ("include_recall", "false"),
                            ("tags", "missing"),
                            ("context", &context),
                        ],
                    ))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: serde_json::Value = response.json().await.unwrap();
                assert_eq!(
                    body["canonical"]["mandatory"][0]["id"],
                    Uuid::from_u128(expected_id).to_string()
                );
                assert_eq!(body["canonical"]["effective"].as_array().unwrap().len(), 1);
            }
            let context = format!("{{\"profile\":\"{profile}\"}}");
            let response = client
                .get(url("guardrails", &[("context", &context)]))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[reqwest::header::CACHE_CONTROL],
                "no-store"
            );
            let pack: memory_common::guardrails::GuardrailPack = response.json().await.unwrap();
            assert_eq!(pack.mandatory.len(), 1);
            assert_eq!(pack.mandatory[0].id, Uuid::from_u128(expected_id));
            pack.validate_for(
                "app",
                &memory_common::policy::ResolutionContext {
                    profile: Some(profile.to_owned()),
                    ..memory_common::policy::ResolutionContext::default()
                },
            )
            .unwrap();
        }
        for endpoint in ["rules", "bootstrap"] {
            let response = client
                .get(url(endpoint, &[("include_recall", "false")]))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "policy_context_required");
            assert_eq!(
                body["error"]["details"]["references"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
        let response = client.get(url("guardrails", &[])).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "policy_context_required");

        let mut incompatible = policy_request(None, 1).policy.unwrap();
        incompatible.policy_key = "storage.other".to_owned();
        incompatible.delivery_class = DeliveryClass::Mandatory;
        incompatible.selectors.profile = Some(BTreeSet::from(["workstation-host".to_owned()]));
        incompatible
            .values
            .insert("build.target".to_owned(), "other".to_owned());
        crate::policy::publish_new(&pool, &scoped_memory("app", 4), &incompatible)
            .await
            .unwrap();
        let response = client
            .get(url(
                "rules",
                &[
                    ("context", r#"{"profile":"workstation-host"}"#),
                    ("tags", "missing"),
                ],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "policy_value_conflict");
        assert_eq!(
            body["error"]["details"]["detail"]["setting_key"],
            "build.target"
        );

        incompatible.revision = 2;
        incompatible.supersedes = Some(Uuid::from_u128(4));
        incompatible
            .values
            .insert("build.target".to_owned(), "persistent-disk".to_owned());
        crate::policy::publish_new(&pool, &scoped_memory("app", 5), &incompatible)
            .await
            .unwrap();

        let mut collision = policy_request(None, 1).policy.unwrap();
        collision.policy_key = "storage.host".to_owned();
        collision.selectors.profile = Some(BTreeSet::from(["workstation-host".to_owned()]));
        crate::policy::publish_new(&pool, &scoped_memory("app", 3), &collision)
            .await
            .unwrap();
        let response = client
            .get(url(
                "bootstrap",
                &[
                    ("context", r#"{"profile":"workstation-host"}"#),
                    ("include_recall", "false"),
                ],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "policy_mandatory_override");
        assert!(body.get("recall_memories").is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn classified_rule_review_and_delete_conflict_without_side_effects(pool: PgPool) {
        let app = app_with_mock(pool.clone(), spawn_mock_server().await);
        let head = app.store_memory(policy_request(None, 1)).await.unwrap();
        let review = app
            .submit_review(
                head.id,
                None,
                "reviewer".to_owned(),
                "approved".to_owned(),
                "notes".to_owned(),
            )
            .await;
        assert!(
            matches!(review, Err(Error::Policy { code, .. }) if code == "policy_immutable_revision")
        );
        let deletion = app.delete_memory(head.id).await;
        assert!(
            matches!(deletion, Err(Error::Policy { code, .. }) if code == "policy_immutable_revision")
        );
        let decisions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM memories WHERE category = 'decision'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(decisions, 0);
        assert!(db::get(&pool, head.id).await.unwrap().is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn search_workflow_policy_reaches_storage_and_graph(pool: PgPool) {
        let mock_url = spawn_mock_server().await;
        let app = MemoryApp::new(
            pool,
            Arc::new(crate::embed::Client::new(
                mock_url.clone(),
                "test-model".to_owned(),
                None,
                None,
            )),
            "test-model".to_owned(),
            1024,
            reqwest::Client::new(),
            mock_url,
            "test-model".to_owned(),
            1024,
        );
        let mut plans = Vec::new();
        for tags in [vec![], vec![format!("task:{}", Uuid::new_v4())]] {
            plans.push(
                app.store_memory(StoreMemoryRequest {
                    category: Category::Plan,
                    content: "nebula retrieval".to_owned(),
                    policy: None,
                    project: "search-policy".to_owned(),
                    summary: "nebula retrieval".to_owned(),
                    tags: Some(tags),
                })
                .await
                .unwrap(),
            );
        }
        let review = app
            .submit_review(
                plans[1].id,
                None,
                "tester".to_owned(),
                "approved".to_owned(),
                "nebula retrieval".to_owned(),
            )
            .await
            .unwrap()
            .unwrap();
        for category in [None, Some(Category::Plan)] {
            for include in [None, Some(false), Some(true)] {
                let mut request = serde_json::json!({
                    "project": "search-policy", "query": "nebula retrieval",
                    "limit": 20, "min_similarity": 0.1, "graph_hops": 2,
                    "category": category,
                });
                if let Some(include) = include {
                    request["include_workflow_artifacts"] = include.into();
                }
                let outcome = app
                    .search_memories(serde_json::from_value(request).unwrap())
                    .await
                    .unwrap();
                let SearchOutcome::Memories(memories) = outcome else {
                    panic!("expected durable search results");
                };
                let ids: Vec<_> = memories.iter().map(|(memory, _)| memory.id).collect();
                assert!(ids.contains(&plans[0].id));
                assert_eq!(ids.contains(&plans[1].id), include == Some(true));
                if category.is_none() || include != Some(true) {
                    assert_eq!(ids.contains(&review.id), include == Some(true));
                }
            }
        }
    }

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
            policy: None,
            category: Category::Plan,
            content: "old plan content".to_owned(),
            created_at: Utc::now(),
            embedding: vec![0.0; 1024],
            project: "test_proj".to_owned(),
            summary: "test plan summary".to_owned(),
            tags: vec![
                "review-needed".to_owned(),
                format!("task:{}", Uuid::new_v4()),
            ],
            updated_at: Utc::now(),
        };
        crate::db::insert_with_workflow_provenance(&pool, &mem)
            .await
            .unwrap();

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

        let marked: bool =
            sqlx::query_scalar("SELECT workflow_artifact FROM memories WHERE id = $1")
                .bind(review.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(marked, "submit_review must use provenance-aware storage");
        assert!(review.tags.contains(&"review".to_owned()));
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn bootstrap_keeps_stronger_plan_and_review_filter(pool: PgPool) {
        let mock_url = spawn_mock_server().await;
        let app = MemoryApp::new(
            pool.clone(),
            Arc::new(crate::embed::Client::new(
                mock_url.clone(),
                "test-model".to_owned(),
                None,
                None,
            )),
            "test-model".to_owned(),
            1024,
            reqwest::Client::new(),
            mock_url,
            "test-model".to_owned(),
            1024,
        );

        let make_memory = |category, content: &str, tags| Memory {
            id: Uuid::new_v4(),
            policy: None,
            category,
            content: content.to_owned(),
            created_at: Utc::now(),
            embedding: vec![0.0; 1024],
            project: "test_proj".to_owned(),
            summary: content.to_owned(),
            tags,
            updated_at: Utc::now(),
        };
        let task_plan = make_memory(
            Category::Plan,
            "task plan",
            vec![format!("task:{}", Uuid::new_v4())],
        );
        let ordinary_plan = make_memory(Category::Plan, "ordinary plan", Vec::new());
        let ordinary_review = make_memory(
            Category::Decision,
            "ordinary plan review",
            vec![
                "review".to_owned(),
                format!("reviewed-item:{}", ordinary_plan.id),
            ],
        );
        let ordinary_decision = make_memory(Category::Decision, "ordinary decision", Vec::new());
        let review_context = make_memory(
            Category::ErrorFix,
            "ordinary context tagged review",
            vec!["review".to_owned()],
        );
        for memory in [
            &task_plan,
            &ordinary_plan,
            &ordinary_review,
            &ordinary_decision,
            &review_context,
        ] {
            crate::db::insert_with_workflow_provenance(&pool, memory)
                .await
                .unwrap();
        }

        let default_recall = app.recall_project("test_proj", None).await.unwrap();
        assert!(
            default_recall
                .iter()
                .all(|memory| memory.id != task_plan.id)
        );
        assert!(
            default_recall
                .iter()
                .any(|memory| memory.id == ordinary_plan.id)
        );
        let inclusive_recall = app.recall_project("test_proj", Some(true)).await.unwrap();
        assert!(
            inclusive_recall
                .iter()
                .any(|memory| memory.id == task_plan.id)
        );

        let bootstrap = app
            .bootstrap_project("test_proj", false, true)
            .await
            .unwrap();
        assert!(bootstrap.recall_memories.iter().all(|memory| {
            memory.category != Category::Plan
                && !(memory.category == Category::Decision
                    && memory.tags.iter().any(|tag| tag == "review"))
        }));
        assert!(
            bootstrap
                .recall_memories
                .iter()
                .any(|memory| memory.id == ordinary_decision.id)
        );
        assert!(
            bootstrap
                .recall_memories
                .iter()
                .any(|memory| memory.id == review_context.id)
        );
        assert!(
            bootstrap
                .recall_memories
                .iter()
                .all(|memory| memory.id != ordinary_review.id)
        );
    }
}
