use std::fmt::Write;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::app::{
    ListMemoriesRequest, MemoryApp, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};
use crate::error::Error;
use crate::http_client::HttpMemoryClient;
use crate::model;
use crate::model::Category;

#[derive(Clone)]
pub enum MemoryBackend {
    Local(MemoryApp),
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
pub struct SearchParams {
    /// Filter by category: `context`, `decision`, `error_fix`, `plan`, `rule`
    category: Option<Category>,
    /// Maximum number of results (default: 5, max: 100)
    limit: Option<i64>,
    /// Minimum similarity threshold (default: 0.5, range: 0.0-1.0)
    min_similarity: Option<f64>,
    /// Project name to search within
    project: String,
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

    #[tool(description = "Semantic memory search: embed query, cosine similarity retrieval")]
    async fn memory_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self
            .backend
            .search_memories(SearchMemoriesRequest {
                category: params.category,
                limit: params.limit,
                min_similarity: params.min_similarity,
                project: params.project,
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
        description = "Store a new memory with category, project, summary, content, and optional tags"
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
        description = "Partial update of memory content/summary/tags, re-embeds using full stored text if any text field changes"
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
}

impl MemoryBackend {
    async fn version(&self) -> Result<String, Error> {
        match self {
            Self::Local(app) => Ok(app.version()),
            Self::Http(client) => client.version().await,
        }
    }

    async fn delete_memory(&self, id: Uuid) -> Result<bool, Error> {
        match self {
            Self::Local(app) => app.delete_memory(id).await,
            Self::Http(client) => client.delete_memory(id).await,
        }
    }

    async fn get_memory(&self, id: Uuid) -> Result<Option<model::MemorySummary>, Error> {
        match self {
            Self::Local(app) => app.get_memory(id).await,
            Self::Http(client) => client.get_memory(id).await,
        }
    }

    async fn list_memories(
        &self,
        request: ListMemoriesRequest,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        match self {
            Self::Local(app) => app.list_memories(request).await,
            Self::Http(client) => client.list_memories(request).await,
        }
    }

    async fn recall_project(&self, project: &str) -> Result<Vec<model::MemorySummary>, Error> {
        match self {
            Self::Local(app) => app.recall_project(project).await,
            Self::Http(client) => client.recall_project(project).await,
        }
    }

    async fn search_memories(
        &self,
        request: SearchMemoriesRequest,
    ) -> Result<SearchOutcome, Error> {
        match self {
            Self::Local(app) => app.search_memories(request).await,
            Self::Http(client) => client.search_memories(request).await,
        }
    }

    async fn store_memory(
        &self,
        request: StoreMemoryRequest,
    ) -> Result<model::MemorySummary, Error> {
        match self {
            Self::Local(app) => app.store_memory(request).await,
            Self::Http(client) => client.store_memory(request).await,
        }
    }

    async fn update_memory(
        &self,
        request: UpdateMemoryRequest,
    ) -> Result<Option<model::MemorySummary>, Error> {
        match self {
            Self::Local(app) => app.update_memory(request).await,
            Self::Http(client) => client.update_memory(request).await,
        }
    }

    async fn store_session_log(&self, request: StoreSessionLogRequest) -> Result<usize, Error> {
        match self {
            Self::Local(app) => app.store_session_log(request).await,
            Self::Http(client) => client.store_session_log(request).await,
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

    use crate::api::{HealthResponse, MemoryEnvelope, MemoryListEnvelope, PatchMemoryRequest};
    use crate::app::outer_rrf;

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

    #[test]
    fn outer_rrf_single_variant() {
        let id = Uuid::from_u128(1);
        let results = vec![vec![(sample_summary_with_id(id), 0.9)]];
        let fused = outer_rrf(&results, 5);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].0.id, id);
        // Original query (index 0) gets 2x weight: 2.0 / (60 + 1) = 2/61
        let expected = 2.0 / 61.0;
        assert!((fused[0].1 - expected).abs() < 1e-10);
    }

    #[test]
    fn outer_rrf_deduplicates() {
        let id = Uuid::from_u128(1);
        let variant0 = vec![(sample_summary_with_id(id), 0.9)];
        let variant1 = vec![(sample_summary_with_id(id), 0.8)];
        let results = vec![variant0, variant1];
        let fused = outer_rrf(&results, 5);
        assert_eq!(fused.len(), 1);
        // 2.0/(60+1) + 1.0/(60+1)
        let expected = 2.0 / 61.0 + 1.0 / 61.0;
        assert!((fused[0].1 - expected).abs() < 1e-10);
    }

    #[test]
    fn outer_rrf_original_weighted_higher() {
        let id_a = Uuid::from_u128(1);
        let id_b = Uuid::from_u128(2);
        // id_a only in original (2x weight), id_b only in expansion (1x weight)
        let variant0 = vec![(sample_summary_with_id(id_a), 0.9)];
        let variant1 = vec![(sample_summary_with_id(id_b), 0.9)];
        let results = vec![variant0, variant1];
        let fused = outer_rrf(&results, 5);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0.id, id_a);
        assert_eq!(fused[1].0.id, id_b);
    }

    #[test]
    fn outer_rrf_respects_limit() {
        let ids: Vec<Uuid> = (1..=5).map(Uuid::from_u128).collect();
        let variant0: Vec<_> = ids
            .iter()
            .map(|&id| (sample_summary_with_id(id), 0.9))
            .collect();
        let results = vec![variant0];
        let fused = outer_rrf(&results, 3);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn outer_rrf_empty_input() {
        let results: Vec<Vec<(model::MemorySummary, f64)>> = vec![vec![]];
        let fused = outer_rrf(&results, 5);
        assert!(fused.is_empty());
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

    #[derive(Clone)]
    struct StubState {
        memory: Arc<Mutex<Option<crate::api::MemoryDto>>>,
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
        let id = extract_uuid(&store_text);

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
        let memory = crate::api::MemoryDto {
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
