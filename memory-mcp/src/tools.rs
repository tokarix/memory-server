use std::fmt::Write;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::error::Error;
use crate::http_client::HttpMemoryClient;
use crate::model;
use crate::model::Category;
use crate::protocol::{
    AppendSessionMessageRequest, BootstrapPayload, CreateSessionRequest, FinalizeSessionRequest,
    ListMemoriesRequest, RuleList, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};

#[derive(Clone)]
pub enum MemoryBackend {
    Http(HttpMemoryClient),
}

pub struct MemoryServer {
    backend: MemoryBackend,
    pub tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    /// UUID of the memory to delete
    id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// UUID of the memory to retrieve
    id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNeighborsParams {
    /// UUID of the memory to find neighbors for
    id: Uuid,
    /// Maximum number of neighbors to return (default: 20, max: 100)
    limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Filter by category: `context`, `decision`, `error_fix`, `plan`, `rule`
    category: Option<Category>,
    /// Maximum number of results (default: 20, max: 100)
    limit: Option<i64>,
    /// Offset for pagination (default: 0)
    offset: Option<i64>,
    /// Project name to list memories for
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallParams {
    /// Project name to recall core memories for
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesParams {
    /// Whether to include rules stored under the shared `general` project
    include_general: Option<bool>,
    /// Project name to load rules for
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BootstrapParams {
    /// Whether to include rules stored under the shared `general` project
    include_general: Option<bool>,
    /// Whether to include non-rule core recall memories alongside rules
    include_recall: Option<bool>,
    /// Project name to bootstrap
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Filter by category: `context`, `decision`, `error_fix`, `plan`, `rule`
    category: Option<Category>,
    /// Allow graph expansion into foreign projects (default: false)
    cross_project: Option<bool>,
    /// Number of graph hops for expansion (default: 1)
    graph_hops: Option<u32>,
    /// Include edges to/from the `general` project during expansion (default: false)
    include_general: Option<bool>,
    /// Maximum number of results (default: 5, max: 100)
    limit: Option<i64>,
    /// Minimum similarity threshold (default: 0.5, range: 0.0-1.0)
    min_similarity: Option<f64>,
    /// Project name to search within
    project: String,
    /// Restrict cross-project expansion to these projects only
    project_allowlist: Option<Vec<String>>,
    /// Natural language search query
    query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionLogStoreParams {
    /// Full session transcript content
    content: String,
    /// Working directory of the session
    cwd: Option<String>,
    /// Project name
    project: Option<String>,
    /// Session identifier
    session_id: String,
    /// Brief summary for embedding
    summary: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionStartParams {
    /// Agent/client identity, for example `claude` or `codex`
    agent: Option<String>,
    /// Working directory of the session
    cwd: Option<String>,
    /// External client session identifier
    external_session_id: String,
    /// Project name
    project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionMessageParams {
    /// Agent/client identity, for example `claude` or `codex`
    agent: Option<String>,
    /// Message body
    content: String,
    /// Optional message kind such as `message`, `tool`, or `command`
    kind: Option<String>,
    /// Optional metadata blob, stored as text
    metadata: Option<String>,
    /// Role such as `user`, `assistant`, `tool`, or `system`
    role: String,
    /// Internal memoryd session UUID
    session_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionFinalizeParams {
    /// Internal memoryd session UUID
    session_id: Uuid,
    /// Optional override summary
    summary: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewQueueParams {
    /// Filter by category: `context`, `decision`, `error_fix`, `plan`, `rule`
    category: Option<Category>,
    /// Maximum number of items to return (default: 20, max: 100)
    limit: Option<i64>,
    /// Project name
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitReviewParams {
    /// Memory UUID to review
    memory_id: Uuid,
    /// Review notes
    notes: String,
    /// Optional override project for the review memory
    project: Option<String>,
    /// Reviewer identity
    reviewer: String,
    /// Verdict string such as `approved`, `changes-requested`, or `rejected`
    verdict: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreParams {
    /// Memory category: `context`, `decision`, `error_fix`, `plan`, or `rule`
    category: Category,
    /// Full content of the memory
    content: String,
    /// Project this memory belongs to
    project: String,
    /// Brief summary for display and embedding
    summary: String,
    /// Optional tags for organization
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateParams {
    /// Updated content (if changing)
    content: Option<String>,
    /// UUID of the memory to update
    id: Uuid,
    /// Updated summary (if changing)
    summary: Option<String>,
    /// Updated tags (if changing)
    tags: Option<Vec<String>>,
}

#[tool_router]
impl MemoryServer {
    #[must_use]
    pub fn new(backend: MemoryBackend) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Return the memory server version (includes git hash)")]
    async fn memory_server_version(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(
            self.backend
                .version()
                .await
                .map_err(rmcp::ErrorData::from)?,
        )]))
    }

    #[tool(description = "Delete a memory by UUID")]
    async fn memory_delete(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let deleted = self
            .backend
            .delete_memory(params.id)
            .await
            .map_err(rmcp::ErrorData::from)?;
        if deleted {
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Deleted memory {}",
                params.id
            ))]))
        } else {
            Ok(CallToolResult::error(vec![Content::text(format!(
                "Memory {} not found",
                params.id
            ))]))
        }
    }

    #[tool(description = "Retrieve a single memory by UUID")]
    async fn memory_get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let memory = self
            .backend
            .get_memory(params.id)
            .await
            .map_err(rmcp::ErrorData::from)?;
        match memory {
            Some(m) => {
                let text = format_single_memory(&m);
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Memory {} not found",
                params.id
            ))])),
        }
    }

    #[tool(description = "Browse memories by project with optional category filter, paginated")]
    async fn memory_list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let memories = self
            .backend
            .list_memories(ListMemoriesRequest {
                category: params.category,
                limit: params.limit,
                offset: params.offset,
                project: params.project,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;

        if memories.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No memories found.",
            )]));
        }

        let text = format_memory_list(&memories);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "List neighbor memories reachable via graph edges from a given memory")]
    async fn memory_neighbors(
        &self,
        Parameters(params): Parameters<ListNeighborsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let neighbors = self
            .backend
            .list_neighbors(params.id, params.limit)
            .await
            .map_err(rmcp::ErrorData::from)?;
        if neighbors.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No neighbors found.",
            )]));
        }
        Ok(CallToolResult::success(vec![Content::text(
            format_neighbor_list(&neighbors),
        )]))
    }

    #[tool(description = "Recall core memories (importance >= 0.7) for a project at session start")]
    async fn memory_recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let memories = self
            .backend
            .recall_project(&params.project)
            .await
            .map_err(rmcp::ErrorData::from)?;

        if memories.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No core memories found.",
            )]));
        }

        let text = format_memory_list(&memories);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Load durable rule memories for a project, optionally unioned with shared general rules"
    )]
    async fn memory_rules(
        &self,
        Parameters(params): Parameters<RulesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rules = self
            .backend
            .list_rules(&params.project, params.include_general.unwrap_or(true))
            .await
            .map_err(rmcp::ErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            format_rule_list(&params.project, &rules),
        )]))
    }

    #[tool(
        description = "Bootstrap a session by loading effective rules plus non-rule core recall memories for a project"
    )]
    async fn memory_bootstrap(
        &self,
        Parameters(params): Parameters<BootstrapParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let payload = self
            .backend
            .bootstrap_project(
                &params.project,
                params.include_general.unwrap_or(true),
                params.include_recall.unwrap_or(true),
            )
            .await
            .map_err(rmcp::ErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            format_bootstrap(&payload),
        )]))
    }

    #[tool(description = "Semantic memory search: embed query, cosine similarity retrieval")]
    async fn memory_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self
            .backend
            .search_memories(SearchMemoriesRequest {
                category: params.category,
                cross_project: params.cross_project,
                graph_hops: params.graph_hops,
                include_general: params.include_general,
                limit: params.limit,
                min_similarity: params.min_similarity,
                project: params.project,
                project_allowlist: params.project_allowlist,
                query: params.query,
            })
            .await
            .map_err(rmcp::ErrorData::from)?
        {
            SearchOutcome::Memories(results) => Ok(CallToolResult::success(vec![Content::text(
                format_search_results(&results),
            )])),
            SearchOutcome::SessionLogs(results) => {
                Ok(CallToolResult::success(vec![Content::text(
                    format_session_log_results(&results),
                )]))
            }
            SearchOutcome::Empty => Ok(CallToolResult::success(vec![Content::text(
                "No matching memories found.",
            )])),
        }
    }

    #[tool(
        description = "Store a new memory. Each memory should cover one concept — prefer creating focused memories over expanding existing ones. Mentioning a UUID in content or using structural tags (plan:<uuid>, reviewed-item:<uuid>) creates graph edges at write time. Shared topical tags and embedding similarity create edges during maintenance."
    )]
    async fn memory_store(
        &self,
        Parameters(params): Parameters<StoreParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let memory = self
            .backend
            .store_memory(StoreMemoryRequest {
                category: params.category,
                content: params.content,
                project: params.project,
                summary: params.summary,
                tags: params.tags,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Stored memory {} ({}): {}",
            memory.id, memory.category, memory.summary
        ))]))
    }

    #[tool(
        description = "Update a memory's content/summary/tags (re-embeds if text changes). Use for corrections or refinements to the same concept — if the new information is a distinct concept, store a new memory instead."
    )]
    async fn memory_update(
        &self,
        Parameters(params): Parameters<UpdateParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if self
            .backend
            .update_memory(UpdateMemoryRequest {
                content: params.content,
                id: params.id,
                summary: params.summary,
                tags: params.tags,
            })
            .await
            .map_err(rmcp::ErrorData::from)?
            .is_some()
        {
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Updated memory {}",
                params.id
            ))]))
        } else {
            Ok(CallToolResult::error(vec![Content::text(format!(
                "Memory {} not found",
                params.id
            ))]))
        }
    }

    #[tool(description = "Store a session log transcript for searchable archival")]
    async fn session_log_store(
        &self,
        Parameters(params): Parameters<SessionLogStoreParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let chunk_count = self
            .backend
            .store_session_log(StoreSessionLogRequest {
                content: params.content,
                cwd: params.cwd,
                project: params.project,
                session_id: params.session_id.clone(),
                summary: params.summary,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Stored session log for session {} ({} chunks)",
            params.session_id, chunk_count
        ))]))
    }

    #[tool(description = "Create or upsert a normalized shared session for cross-agent capture")]
    async fn session_start(
        &self,
        Parameters(params): Parameters<SessionStartParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session = self
            .backend
            .create_session(CreateSessionRequest {
                agent: params.agent,
                cwd: params.cwd,
                external_session_id: params.external_session_id,
                project: params.project,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Started session {} for external session {}",
            session.id, session.external_session_id
        ))]))
    }

    #[tool(description = "Append one message or tool event to a normalized shared session")]
    async fn session_message_append(
        &self,
        Parameters(params): Parameters<SessionMessageParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let message = self
            .backend
            .append_session_message(AppendSessionMessageRequest {
                agent: params.agent,
                content: params.content,
                kind: params.kind,
                metadata: params.metadata,
                role: params.role,
                session_id: params.session_id,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Appended {} message {} to session {}",
            message.role, message.id, message.session_id
        ))]))
    }

    #[tool(description = "Finalize a normalized session into searchable session-log chunks")]
    async fn session_finalize(
        &self,
        Parameters(params): Parameters<SessionFinalizeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let chunk_count = self
            .backend
            .finalize_session(FinalizeSessionRequest {
                session_id: params.session_id,
                summary: params.summary,
            })
            .await
            .map_err(rmcp::ErrorData::from)?;
        match chunk_count {
            Some(count) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Finalized session {} ({} chunks)",
                params.session_id, count
            ))])),
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Session {} not found",
                params.session_id
            ))])),
        }
    }

    #[tool(
        description = "List memories in a project that are tagged review-needed, with optional category filter"
    )]
    async fn review_queue(
        &self,
        Parameters(params): Parameters<ReviewQueueParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let limit = params.limit.unwrap_or(20).clamp(1, 100);
        let memories = self
            .backend
            .list_review_queue(&params.project, params.category.as_ref(), limit)
            .await
            .map_err(rmcp::ErrorData::from)?;
        if memories.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No items awaiting review.",
            )]));
        }
        Ok(CallToolResult::success(vec![Content::text(
            format_memory_list(&memories),
        )]))
    }

    #[tool(description = "Store a review decision for a memory and mark the original reviewed")]
    async fn review_submit(
        &self,
        Parameters(params): Parameters<SubmitReviewParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let review = self
            .backend
            .submit_review(
                params.memory_id,
                params.project,
                params.reviewer,
                params.verdict,
                params.notes,
            )
            .await
            .map_err(rmcp::ErrorData::from)?;
        match review {
            Some(memory) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Stored review {} for memory {}",
                memory.id, params.memory_id
            ))])),
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Memory {} not found",
                params.memory_id
            ))])),
        }
    }
}

impl MemoryBackend {
    async fn version(&self) -> Result<String, Error> {
        match self {
            Self::Http(client) => client.version().await,
        }
    }

    async fn delete_memory(&self, id: Uuid) -> Result<bool, Error> {
        match self {
            Self::Http(client) => client.delete_memory(id).await,
        }
    }

    async fn get_memory(&self, id: Uuid) -> Result<Option<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => client.get_memory(id).await,
        }
    }

    async fn list_neighbors(
        &self,
        memory_id: Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<(model::MemoryEdgeSummary, model::MemorySummary)>, Error> {
        match self {
            Self::Http(client) => client.list_neighbors(memory_id, limit).await,
        }
    }

    async fn list_memories(
        &self,
        request: ListMemoriesRequest,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => client.list_memories(request).await,
        }
    }

    async fn recall_project(&self, project: &str) -> Result<Vec<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => client.recall_project(project).await,
        }
    }

    async fn list_rules(&self, project: &str, include_general: bool) -> Result<RuleList, Error> {
        match self {
            Self::Http(client) => client.list_rules(project, include_general).await,
        }
    }

    async fn bootstrap_project(
        &self,
        project: &str,
        include_general: bool,
        include_recall: bool,
    ) -> Result<BootstrapPayload, Error> {
        match self {
            Self::Http(client) => {
                client
                    .bootstrap_project(project, include_general, include_recall)
                    .await
            }
        }
    }

    async fn search_memories(
        &self,
        request: SearchMemoriesRequest,
    ) -> Result<SearchOutcome, Error> {
        match self {
            Self::Http(client) => client.search_memories(request).await,
        }
    }

    async fn store_memory(
        &self,
        request: StoreMemoryRequest,
    ) -> Result<model::MemorySummary, Error> {
        match self {
            Self::Http(client) => client.store_memory(request).await,
        }
    }

    async fn update_memory(
        &self,
        request: UpdateMemoryRequest,
    ) -> Result<Option<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => client.update_memory(request).await,
        }
    }

    async fn store_session_log(&self, request: StoreSessionLogRequest) -> Result<usize, Error> {
        match self {
            Self::Http(client) => client.store_session_log(request).await,
        }
    }

    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<model::SessionSummary, Error> {
        match self {
            Self::Http(client) => client.create_session(request).await,
        }
    }

    async fn append_session_message(
        &self,
        request: AppendSessionMessageRequest,
    ) -> Result<model::SessionMessageSummary, Error> {
        match self {
            Self::Http(client) => client.append_session_message(request).await,
        }
    }

    async fn finalize_session(
        &self,
        request: FinalizeSessionRequest,
    ) -> Result<Option<usize>, Error> {
        match self {
            Self::Http(client) => client.finalize_session(request).await,
        }
    }

    async fn list_review_queue(
        &self,
        project: &str,
        category: Option<&model::Category>,
        limit: i64,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => client.list_review_queue(project, category, limit).await,
        }
    }

    async fn submit_review(
        &self,
        memory_id: Uuid,
        project: Option<String>,
        reviewer: String,
        verdict: String,
        notes: String,
    ) -> Result<Option<model::MemorySummary>, Error> {
        match self {
            Self::Http(client) => {
                client
                    .submit_review(memory_id, project, reviewer, verdict, notes)
                    .await
            }
        }
    }
}

fn format_single_memory(m: &model::MemorySummary) -> String {
    format!(
        "## [{}] {} (importance: {:.2})\nID: {}\nProject: {}\nTags: {}\nCreated: {}\nUpdated: {}\n\n{}",
        m.category,
        m.summary,
        m.category.importance(),
        m.id,
        m.project,
        m.tags.join(", "),
        m.created_at.format("%Y-%m-%d %H:%M"),
        m.updated_at.format("%Y-%m-%d %H:%M"),
        m.content,
    )
}

fn format_memory_list(memories: &[model::MemorySummary]) -> String {
    let mut out = format!("## Memories ({} results)\n", memories.len());
    for (i, m) in memories.iter().enumerate() {
        let _ = write!(
            out,
            "\n### {}. [{}] {} (importance: {:.2})\nID: {}\nTags: {}\nCreated: {}\nUpdated: {}\n\n{}\n\n---\n",
            i + 1,
            m.category,
            m.summary,
            m.category.importance(),
            m.id,
            m.tags.join(", "),
            m.created_at.format("%Y-%m-%d %H:%M"),
            m.updated_at.format("%Y-%m-%d %H:%M"),
            m.content,
        );
    }
    out
}

fn format_search_results(results: &[(model::MemorySummary, f64)]) -> String {
    let mut out = format!("## Search Results ({} matches)\n", results.len());
    for (i, (m, similarity)) in results.iter().enumerate() {
        let _ = write!(
            out,
            "\n### {}. [{}] {} (importance: {:.2}, similarity: {:.2})\nID: {}\nTags: {}\nCreated: {}\n\n{}\n\n---\n",
            i + 1,
            m.category,
            m.summary,
            m.category.importance(),
            similarity,
            m.id,
            m.tags.join(", "),
            m.created_at.format("%Y-%m-%d %H:%M"),
            m.content,
        );
    }
    out
}

fn format_rule_list(project: &str, rules: &RuleList) -> String {
    let total = rules.general_rules.len() + rules.project_rules.len();
    let mut out = format!("## Rule Set ({total} rules)\nProject: {project}\n");
    append_memory_section(&mut out, "General Rules", &rules.general_rules);
    append_memory_section(&mut out, "Project Rules", &rules.project_rules);
    out
}

fn format_bootstrap(payload: &BootstrapPayload) -> String {
    let mut out = format!("## Bootstrap\nProject: {}\n", payload.project);
    append_memory_section(&mut out, "General Rules", &payload.general_rules);
    append_memory_section(&mut out, "Project Rules", &payload.project_rules);
    append_memory_section(&mut out, "Core Recall", &payload.recall_memories);
    out
}

fn append_memory_section(out: &mut String, title: &str, memories: &[model::MemorySummary]) {
    let _ = write!(out, "\n### {title} ({})\n", memories.len());
    if memories.is_empty() {
        let _ = writeln!(out, "None.");
        return;
    }
    for (index, memory) in memories.iter().enumerate() {
        let _ = write!(
            out,
            "\n{}. [{}] {}\nProject: {}\nTags: {}\nUpdated: {}\n\n{}\n",
            index + 1,
            memory.category,
            memory.summary,
            memory.project,
            memory.tags.join(", "),
            memory.updated_at.format("%Y-%m-%d %H:%M"),
            memory.content,
        );
    }
}

fn format_neighbor_list(neighbors: &[(model::MemoryEdgeSummary, model::MemorySummary)]) -> String {
    let mut out = format!("## Neighbors ({} results)\n", neighbors.len());
    for (i, (edge, memory)) in neighbors.iter().enumerate() {
        let _ = write!(
            out,
            "\n### {}. [{}] {} (weight: {:.2})\nID: {}\nEdge: {} via {} (confidence: {:.2})\nProject: {}\nTags: {}\n\n{}\n\n---\n",
            i + 1,
            memory.category,
            memory.summary,
            edge.weight,
            memory.id,
            edge.relation,
            edge.origin,
            edge.confidence,
            memory.project,
            memory.tags.join(", "),
            memory.content,
        );
    }
    out
}

fn format_session_log_results(results: &[(model::SessionLogSummary, f64)]) -> String {
    let mut out = format!(
        "## Session Log Results (fallback, {} matches)\n",
        results.len()
    );
    for (i, (log, similarity)) in results.iter().enumerate() {
        let _ = write!(
            out,
            "\n### {}. {} (similarity: {:.2})\nSession: {}\nProject: {}\nCwd: {}\nCreated: {}\n\n{}\n\n---\n",
            i + 1,
            log.summary,
            similarity,
            log.session_id,
            log.project,
            log.cwd,
            log.created_at.format("%Y-%m-%d %H:%M"),
            log.content,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use chrono::{TimeZone, Utc};
    use tokio::net::TcpListener;

    use crate::protocol::{
        BootstrapEnvelope, HealthResponse, MemoryEnvelope, MemoryListEnvelope, PatchMemoryRequest,
        RuleListEnvelope,
    };

    use super::*;

    fn sample_summary() -> model::MemorySummary {
        sample_summary_with_id(Uuid::nil())
    }

    fn sample_summary_with_id(id: Uuid) -> model::MemorySummary {
        model::MemorySummary {
            id,
            category: Category::Decision,
            content: "Use pgvector for semantic search.".to_owned(),
            created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            project: "test".to_owned(),
            summary: "Choose pgvector".to_owned(),
            tags: vec!["database".to_owned(), "architecture".to_owned()],
            updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn format_list_output() {
        let memories = vec![sample_summary()];
        let output = format_memory_list(&memories);
        assert!(output.contains("## Memories (1 results)"));
        assert!(output.contains("[decision] Choose pgvector (importance: 0.75)"));
        assert!(output.contains("Use pgvector for semantic search."));
        assert!(output.contains("database, architecture"));
    }

    #[test]
    fn format_search_output() {
        let results = vec![(sample_summary(), 0.89)];
        let output = format_search_results(&results);
        assert!(output.contains("## Search Results (1 matches)"));
        assert!(output.contains("(importance: 0.75, similarity: 0.89)"));
        assert!(output.contains("[decision] Choose pgvector"));
    }

    #[test]
    fn format_list_empty() {
        let output = format_memory_list(&[]);
        assert!(output.contains("0 results"));
    }

    #[test]
    fn format_search_empty() {
        let output = format_search_results(&[]);
        assert!(output.contains("0 matches"));
    }

    #[test]
    fn format_single() {
        let m = sample_summary();
        let output = format_single_memory(&m);
        assert!(output.contains("## [decision] Choose pgvector (importance: 0.75)"));
        assert!(output.contains("Project: test"));
        assert!(output.contains("Use pgvector for semantic search."));
    }

    fn sample_session_log() -> model::SessionLogSummary {
        model::SessionLogSummary {
            id: Uuid::nil(),
            content: "User: How do I fix the build?\nAssistant: Run cargo clean.".to_owned(),
            created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            cwd: "/home/user/myproject".to_owned(),
            project: "myproject".to_owned(),
            session_id: "sess-123".to_owned(),
            summary: "Fix the build".to_owned(),
        }
    }

    #[test]
    fn format_session_log_output() {
        let results = vec![(sample_session_log(), 0.75)];
        let output = format_session_log_results(&results);
        assert!(output.contains("Session Log Results (fallback, 1 matches)"));
        assert!(output.contains("(similarity: 0.75)"));
        assert!(output.contains("Session: sess-123"));
        assert!(output.contains("Project: myproject"));
        assert!(output.contains("cargo clean"));
    }

    #[test]
    fn format_session_log_empty() {
        let output = format_session_log_results(&[]);
        assert!(output.contains("0 matches"));
    }

    #[test]
    fn format_rule_list_output() {
        let general = model::MemorySummary {
            project: "general".to_owned(),
            summary: "Always run tests".to_owned(),
            ..sample_summary()
        };
        let project = sample_summary();
        let output = format_rule_list(
            "test",
            &RuleList {
                general_rules: vec![general],
                project_rules: vec![project],
            },
        );
        assert!(output.contains("## Rule Set (2 rules)"));
        assert!(output.contains("### General Rules (1)"));
        assert!(output.contains("### Project Rules (1)"));
        assert!(output.contains("Always run tests"));
        assert!(output.contains("Choose pgvector"));
    }

    #[test]
    fn format_bootstrap_output() {
        let output = format_bootstrap(&BootstrapPayload {
            general_rules: vec![],
            project: "test".to_owned(),
            project_rules: vec![sample_summary()],
            recall_memories: vec![sample_summary_with_id(Uuid::from_u128(2))],
        });
        assert!(output.contains("## Bootstrap"));
        assert!(output.contains("### General Rules (0)"));
        assert!(output.contains("None."));
        assert!(output.contains("### Project Rules (1)"));
        assert!(output.contains("### Core Recall (1)"));
    }

    #[derive(Clone)]
    struct StubState {
        memory: Arc<Mutex<Option<crate::protocol::MemoryDto>>>,
    }

    #[tokio::test]
    async fn http_backend_mcp_store_get_update_roundtrip() {
        let state = StubState {
            memory: Arc::new(Mutex::new(None)),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/api/v1/health", get(stub_health))
            .route("/api/v1/memories", post(stub_store_memory))
            .route(
                "/api/v1/memories/{id}",
                get(stub_get_memory).patch(stub_update_memory),
            )
            .route(
                "/api/v1/projects/{project}/memories",
                get(stub_list_memories),
            )
            .route("/api/v1/projects/{project}/rules", get(stub_list_rules))
            .route("/api/v1/projects/{project}/bootstrap", get(stub_bootstrap))
            .with_state(state.clone());
        let _server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = HttpMemoryClient::new(&format!("http://{addr}"), None).unwrap();
        let server = MemoryServer::new(MemoryBackend::Http(client));

        let store = server
            .memory_store(Parameters(StoreParams {
                category: Category::Decision,
                content: "Use memoryd behind the MCP adapter.".to_owned(),
                project: "memory-server".to_owned(),
                summary: "Split MCP from HTTP service".to_owned(),
                tags: Some(vec!["split".to_owned(), "http".to_owned()]),
            }))
            .await
            .unwrap();
        let store_text = first_text(&store);
        assert!(store_text.contains("Stored memory"));
        let id = extract_uuid(store_text);

        let get = server
            .memory_get(Parameters(GetParams { id }))
            .await
            .unwrap();
        let get_text = first_text(&get);
        assert!(get_text.contains("Split MCP from HTTP service"));
        assert!(get_text.contains("Use memoryd behind the MCP adapter."));

        let update = server
            .memory_update(Parameters(UpdateParams {
                content: Some("Use memoryd over HTTP for the MCP adapter.".to_owned()),
                id,
                summary: Some("HTTP adapter uses exact API payloads".to_owned()),
                tags: Some(vec![
                    "split".to_owned(),
                    "http".to_owned(),
                    "tested".to_owned(),
                ]),
            }))
            .await
            .unwrap();
        let update_text = first_text(&update);
        assert!(update_text.contains("Updated memory"));

        let get_after = server
            .memory_get(Parameters(GetParams { id }))
            .await
            .unwrap();
        let get_after_text = first_text(&get_after);
        assert!(get_after_text.contains("HTTP adapter uses exact API payloads"));
        assert!(get_after_text.contains("Use memoryd over HTTP for the MCP adapter."));
        assert!(get_after_text.contains("split, http, tested"));

        let rules = server
            .memory_rules(Parameters(RulesParams {
                include_general: Some(true),
                project: "memory-server".to_owned(),
            }))
            .await
            .unwrap();
        let rules_text = first_text(&rules);
        assert!(rules_text.contains("## Rule Set"));
        assert!(rules_text.contains("### General Rules"));
        assert!(rules_text.contains("### Project Rules"));

        let bootstrap = server
            .memory_bootstrap(Parameters(BootstrapParams {
                include_general: Some(true),
                include_recall: Some(true),
                project: "memory-server".to_owned(),
            }))
            .await
            .unwrap();
        let bootstrap_text = first_text(&bootstrap);
        assert!(bootstrap_text.contains("## Bootstrap"));
        assert!(bootstrap_text.contains("### Core Recall"));
    }

    async fn stub_health() -> Json<HealthResponse> {
        Json(HealthResponse {
            status: "ok".to_owned(),
            version: "test-version".to_owned(),
        })
    }

    async fn stub_store_memory(
        State(state): State<StubState>,
        Json(request): Json<StoreMemoryRequest>,
    ) -> Json<MemoryEnvelope> {
        let memory = crate::protocol::MemoryDto {
            category: request.category,
            content: request.content,
            created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            id: Uuid::new_v4(),
            project: request.project,
            summary: request.summary,
            tags: request.tags.unwrap_or_default(),
            updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
        };
        *state.memory.lock().unwrap() = Some(memory.clone());
        Json(MemoryEnvelope { memory })
    }

    async fn stub_get_memory(
        State(state): State<StubState>,
        Path(id): Path<Uuid>,
    ) -> Result<Json<MemoryEnvelope>, axum::http::StatusCode> {
        let memory = state.memory.lock().unwrap().clone();
        match memory {
            Some(memory) if memory.id == id => Ok(Json(MemoryEnvelope { memory })),
            _ => Err(axum::http::StatusCode::NOT_FOUND),
        }
    }

    async fn stub_update_memory(
        State(state): State<StubState>,
        Path(id): Path<Uuid>,
        Json(request): Json<PatchMemoryRequest>,
    ) -> Result<Json<MemoryEnvelope>, axum::http::StatusCode> {
        let mut guard = state.memory.lock().unwrap();
        let Some(memory) = guard.as_mut() else {
            return Err(axum::http::StatusCode::NOT_FOUND);
        };
        if memory.id != id {
            return Err(axum::http::StatusCode::NOT_FOUND);
        }
        if let Some(content) = request.content {
            memory.content = content;
        }
        if let Some(summary) = request.summary {
            memory.summary = summary;
        }
        if let Some(tags) = request.tags {
            memory.tags = tags;
        }
        memory.updated_at = Utc.with_ymd_and_hms(2025, 6, 16, 12, 0, 0).unwrap();
        Ok(Json(MemoryEnvelope {
            memory: memory.clone(),
        }))
    }

    async fn stub_list_memories(
        State(state): State<StubState>,
        Path(_project): Path<String>,
    ) -> Json<MemoryListEnvelope> {
        let memories = state.memory.lock().unwrap().clone().into_iter().collect();
        Json(MemoryListEnvelope { memories })
    }

    async fn stub_list_rules(
        State(state): State<StubState>,
        Path(_project): Path<String>,
    ) -> Json<RuleListEnvelope> {
        let project_rules = state.memory.lock().unwrap().clone().into_iter().collect();
        Json(RuleListEnvelope {
            general_rules: vec![crate::protocol::MemoryDto {
                category: Category::Rule,
                content: "Use hooks to block unsafe actions.".to_owned(),
                created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
                id: Uuid::from_u128(99),
                project: "general".to_owned(),
                summary: "General safety rules".to_owned(),
                tags: vec!["rules".to_owned()],
                updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            }],
            project_rules,
        })
    }

    async fn stub_bootstrap(
        State(state): State<StubState>,
        Path(project): Path<String>,
    ) -> Json<BootstrapEnvelope> {
        let project_rules = state.memory.lock().unwrap().clone().into_iter().collect();
        Json(BootstrapEnvelope {
            general_rules: vec![crate::protocol::MemoryDto {
                category: Category::Rule,
                content: "Use hooks to block unsafe actions.".to_owned(),
                created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
                id: Uuid::from_u128(99),
                project: "general".to_owned(),
                summary: "General safety rules".to_owned(),
                tags: vec!["rules".to_owned()],
                updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            }],
            project,
            project_rules,
            recall_memories: vec![crate::protocol::MemoryDto {
                category: Category::Decision,
                content: "Prefer the HTTP adapter boundary.".to_owned(),
                created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
                id: Uuid::from_u128(100),
                project: "memory-server".to_owned(),
                summary: "Adapter boundary decision".to_owned(),
                tags: vec!["decision".to_owned()],
                updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            }],
        })
    }

    fn first_text(result: &CallToolResult) -> &str {
        result.content[0]
            .raw
            .as_text()
            .map(|text| text.text.as_str())
            .unwrap()
    }

    fn extract_uuid(text: &str) -> Uuid {
        text.split_whitespace()
            .find_map(|token| Uuid::parse_str(token).ok())
            .unwrap()
    }
}
