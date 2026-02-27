use std::fmt::Write;
use std::sync::Arc;

use chrono::Utc;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::embed;
use crate::error::Error;
use crate::model::Category;
use crate::{db, model};

const DEFAULT_MIN_SIMILARITY: f64 = 0.5;

pub struct MemoryServer {
    embed_client: Arc<embed::Client>,
    pool: PgPool,
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
    /// Filter by category: `context`, `decision`, `error_fix`
    category: Option<Category>,
    /// Maximum number of results (default: 20, max: 100)
    limit: Option<i64>,
    /// Offset for pagination (default: 0)
    offset: Option<i64>,
    /// Project name to list memories for
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Filter by category: `context`, `decision`, `error_fix`
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
pub struct StoreParams {
    /// Memory category: `context`, `decision`, or `error_fix`
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
    pub fn new(pool: PgPool, embed_client: Arc<embed::Client>) -> Self {
        Self {
            embed_client,
            pool,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Delete a memory by UUID")]
    async fn memory_delete(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let deleted = db::delete(&self.pool, params.id)
            .await
            .map_err(Error::from)?;
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
        let memory = db::get(&self.pool, params.id).await.map_err(Error::from)?;
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
        let limit = params.limit.unwrap_or(20).clamp(1, 100);
        let offset = params.offset.unwrap_or(0).max(0);
        let memories = db::list(
            &self.pool,
            &params.project,
            params.category.as_ref(),
            limit,
            offset,
        )
        .await
        .map_err(Error::from)?;

        if memories.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No memories found.",
            )]));
        }

        let text = format_memory_list(&memories);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Hybrid memory search: combines semantic vector similarity with full-text keyword matching"
    )]
    async fn memory_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let embedding = self
            .embed_client
            .embed(&params.query, "")
            .await
            .map_err(rmcp::ErrorData::from)?;
        let limit = params.limit.unwrap_or(5).clamp(1, 100);
        let min_similarity = params
            .min_similarity
            .unwrap_or(DEFAULT_MIN_SIMILARITY)
            .clamp(0.0, 1.0);
        let results = db::hybrid_search(
            &self.pool,
            embedding,
            &params.query,
            &params.project,
            params.category.as_ref(),
            limit,
            min_similarity,
        )
        .await
        .map_err(Error::from)?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching memories found.",
            )]));
        }

        let text = format_search_results(&results);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Store a new memory with category, project, summary, content, and optional tags"
    )]
    async fn memory_store(
        &self,
        Parameters(params): Parameters<StoreParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let embedding = self
            .embed_client
            .embed(&params.summary, &params.content)
            .await
            .map_err(rmcp::ErrorData::from)?;
        let now = Utc::now();
        let memory = model::Memory {
            id: Uuid::new_v4(),
            category: params.category,
            content: params.content,
            created_at: now,
            embedding,
            project: params.project,
            summary: params.summary,
            tags: params.tags.unwrap_or_default(),
            updated_at: now,
        };
        db::insert(&self.pool, &memory).await.map_err(Error::from)?;
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
        let embedding = if params.summary.is_some() || params.content.is_some() {
            // Fetch existing record to use stored fields for any field not provided
            let current = db::get(&self.pool, params.id)
                .await
                .map_err(Error::from)?
                .ok_or_else(|| Error::Embedding(format!("memory {} not found", params.id)))?;
            let summary = params.summary.as_deref().unwrap_or(&current.summary);
            let content = params.content.as_deref().unwrap_or(&current.content);
            Some(
                self.embed_client
                    .embed(summary, content)
                    .await
                    .map_err(rmcp::ErrorData::from)?,
            )
        } else {
            None
        };

        let updated = db::update(
            &self.pool,
            params.id,
            params.content.as_deref(),
            embedding,
            params.summary.as_deref(),
            params.tags.as_deref(),
        )
        .await
        .map_err(Error::from)?;

        if updated {
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
}

fn format_single_memory(m: &model::MemorySummary) -> String {
    format!(
        "## [{}] {}\nID: {}\nProject: {}\nTags: {}\nCreated: {}\nUpdated: {}\n\n{}",
        m.category,
        m.summary,
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
            "\n### {}. [{}] {}\nID: {}\nTags: {}\nCreated: {}\nUpdated: {}\n\n{}\n\n---\n",
            i + 1,
            m.category,
            m.summary,
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
            "\n### {}. [{}] {} (similarity: {:.2})\nID: {}\nTags: {}\nCreated: {}\n\n{}\n\n---\n",
            i + 1,
            m.category,
            m.summary,
            similarity,
            m.id,
            m.tags.join(", "),
            m.created_at.format("%Y-%m-%d %H:%M"),
            m.content,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn sample_summary() -> model::MemorySummary {
        model::MemorySummary {
            id: Uuid::nil(),
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
        assert!(output.contains("[decision] Choose pgvector"));
        assert!(output.contains("Use pgvector for semantic search."));
        assert!(output.contains("database, architecture"));
    }

    #[test]
    fn format_search_output() {
        let results = vec![(sample_summary(), 0.89)];
        let output = format_search_results(&results);
        assert!(output.contains("## Search Results (1 matches)"));
        assert!(output.contains("(similarity: 0.89)"));
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
        assert!(output.contains("## [decision] Choose pgvector"));
        assert!(output.contains("Project: test"));
        assert!(output.contains("Use pgvector for semantic search."));
    }
}
