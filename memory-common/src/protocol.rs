use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    Category, EdgeOrigin, EdgeRelation, MemoryEdgeSummary, MemorySummary, SessionLogSummary,
    SessionMessageSummary, SessionSummary,
};

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
    /// Allow graph expansion into foreign projects (default: false)
    pub cross_project: Option<bool>,
    /// Number of graph hops for expansion (default: 1)
    pub graph_hops: Option<u32>,
    /// Include edges to/from the `general` project during expansion (default: false)
    pub include_general: Option<bool>,
    pub limit: Option<i64>,
    pub min_similarity: Option<f64>,
    pub project: String,
    /// Restrict cross-project expansion to these projects only
    pub project_allowlist: Option<Vec<String>>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateSessionRequest {
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub external_session_id: String,
    pub project: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendSessionMessageRequest {
    pub agent: Option<String>,
    pub content: String,
    pub kind: Option<String>,
    pub metadata: Option<String>,
    pub role: String,
    pub session_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FinalizeSessionRequest {
    pub session_id: Uuid,
    pub summary: Option<String>,
}

#[derive(Clone)]
pub struct RuleList {
    pub general_rules: Vec<MemorySummary>,
    pub project_rules: Vec<MemorySummary>,
}

#[derive(Clone)]
pub struct BootstrapPayload {
    pub general_rules: Vec<MemorySummary>,
    pub project: String,
    pub project_rules: Vec<MemorySummary>,
    pub recall_memories: Vec<MemorySummary>,
}

pub enum SearchOutcome {
    Memories(Vec<(MemorySummary, f64)>),
    SessionLogs(Vec<(SessionLogSummary, f64)>),
    Empty,
}

#[derive(Deserialize, Serialize)]
pub struct PatchMemoryRequest {
    pub content: Option<String>,
    pub summary: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MemoryEnvelope {
    pub memory: MemoryDto,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MemoryListEnvelope {
    pub memories: Vec<MemoryDto>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct RuleListEnvelope {
    pub general_rules: Vec<MemoryDto>,
    pub project_rules: Vec<MemoryDto>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct BootstrapEnvelope {
    pub general_rules: Vec<MemoryDto>,
    pub project: String,
    pub project_rules: Vec<MemoryDto>,
    pub recall_memories: Vec<MemoryDto>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SearchEnvelope {
    pub fallback: bool,
    pub memories: Vec<MemoryMatchDto>,
    pub session_logs: Vec<SessionLogMatchDto>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct StoreSessionLogEnvelope {
    pub chunk_count: usize,
    pub session_id: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionEnvelope {
    pub session: SessionDto,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionMessageEnvelope {
    pub message: SessionMessageDto,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct FinalizeSessionEnvelope {
    pub chunk_count: usize,
    pub session_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DeleteEnvelope {
    pub deleted: bool,
    pub id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct EdgeDto {
    pub confidence: f64,
    pub created_at: DateTime<Utc>,
    pub dst_id: Uuid,
    pub dst_project: String,
    pub evidence: Option<String>,
    pub id: Uuid,
    pub origin: EdgeOrigin,
    pub relation: EdgeRelation,
    pub src_id: Uuid,
    pub src_project: String,
    pub suppressed: bool,
    pub updated_at: DateTime<Utc>,
    pub weight: f64,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct NeighborDto {
    pub edge: EdgeDto,
    pub memory: MemoryDto,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct NeighborListEnvelope {
    pub neighbors: Vec<NeighborDto>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MemoryMatchDto {
    pub memory: MemoryDto,
    pub similarity: f64,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionLogMatchDto {
    pub session_log: SessionLogDto,
    pub similarity: f64,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MemoryDto {
    pub category: Category,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
    pub project: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionLogDto {
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub id: Uuid,
    pub project: String,
    pub session_id: String,
    pub summary: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionDto {
    pub agent: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub external_session_id: String,
    pub id: Uuid,
    pub project: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionMessageDto {
    pub agent: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
    pub kind: String,
    pub metadata: Option<String>,
    pub role: String,
    pub session_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct CreateSessionDto {
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub external_session_id: String,
    pub project: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AppendSessionMessageDto {
    pub agent: Option<String>,
    pub content: String,
    pub kind: Option<String>,
    pub metadata: Option<String>,
    pub role: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct FinalizeSessionDto {
    pub summary: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SubmitReviewDto {
    pub memory_id: Uuid,
    pub notes: String,
    pub project: Option<String>,
    pub reviewer: String,
    pub verdict: String,
}

impl From<MemoryEdgeSummary> for EdgeDto {
    fn from(edge: MemoryEdgeSummary) -> Self {
        Self {
            confidence: edge.confidence,
            created_at: edge.created_at,
            dst_id: edge.dst_id,
            dst_project: edge.dst_project,
            evidence: edge.evidence,
            id: edge.id,
            origin: edge.origin,
            relation: edge.relation,
            src_id: edge.src_id,
            src_project: edge.src_project,
            suppressed: edge.suppressed,
            updated_at: edge.updated_at,
            weight: edge.weight,
        }
    }
}

impl From<EdgeDto> for MemoryEdgeSummary {
    fn from(edge: EdgeDto) -> Self {
        Self {
            confidence: edge.confidence,
            created_at: edge.created_at,
            dst_id: edge.dst_id,
            dst_project: edge.dst_project,
            evidence: edge.evidence,
            id: edge.id,
            origin: edge.origin,
            relation: edge.relation,
            src_id: edge.src_id,
            src_project: edge.src_project,
            suppressed: edge.suppressed,
            updated_at: edge.updated_at,
            weight: edge.weight,
        }
    }
}

impl From<MemorySummary> for MemoryDto {
    fn from(memory: MemorySummary) -> Self {
        Self {
            category: memory.category,
            content: memory.content,
            created_at: memory.created_at,
            id: memory.id,
            project: memory.project,
            summary: memory.summary,
            tags: memory.tags,
            updated_at: memory.updated_at,
        }
    }
}

impl From<MemoryDto> for MemorySummary {
    fn from(memory: MemoryDto) -> Self {
        Self {
            category: memory.category,
            content: memory.content,
            created_at: memory.created_at,
            id: memory.id,
            project: memory.project,
            summary: memory.summary,
            tags: memory.tags,
            updated_at: memory.updated_at,
        }
    }
}

impl From<SessionLogSummary> for SessionLogDto {
    fn from(session_log: SessionLogSummary) -> Self {
        Self {
            content: session_log.content,
            created_at: session_log.created_at,
            cwd: session_log.cwd,
            id: session_log.id,
            project: session_log.project,
            session_id: session_log.session_id,
            summary: session_log.summary,
        }
    }
}

impl From<SessionLogDto> for SessionLogSummary {
    fn from(session_log: SessionLogDto) -> Self {
        Self {
            content: session_log.content,
            created_at: session_log.created_at,
            cwd: session_log.cwd,
            id: session_log.id,
            project: session_log.project,
            session_id: session_log.session_id,
            summary: session_log.summary,
        }
    }
}

impl From<SessionSummary> for SessionDto {
    fn from(session: SessionSummary) -> Self {
        Self {
            agent: session.agent,
            created_at: session.created_at,
            cwd: session.cwd,
            ended_at: session.ended_at,
            external_session_id: session.external_session_id,
            id: session.id,
            project: session.project,
            updated_at: session.updated_at,
        }
    }
}

impl From<SessionDto> for SessionSummary {
    fn from(session: SessionDto) -> Self {
        Self {
            agent: session.agent,
            created_at: session.created_at,
            cwd: session.cwd,
            ended_at: session.ended_at,
            external_session_id: session.external_session_id,
            id: session.id,
            project: session.project,
            updated_at: session.updated_at,
        }
    }
}

impl From<SessionMessageSummary> for SessionMessageDto {
    fn from(message: SessionMessageSummary) -> Self {
        Self {
            agent: message.agent,
            content: message.content,
            created_at: message.created_at,
            id: message.id,
            kind: message.kind,
            metadata: message.metadata,
            role: message.role,
            session_id: message.session_id,
        }
    }
}

impl From<SessionMessageDto> for SessionMessageSummary {
    fn from(message: SessionMessageDto) -> Self {
        Self {
            agent: message.agent,
            content: message.content,
            created_at: message.created_at,
            id: message.id,
            kind: message.kind,
            metadata: message.metadata,
            role: message.role,
            session_id: message.session_id,
        }
    }
}
