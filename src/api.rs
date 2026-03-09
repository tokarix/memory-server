use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{
    ListMemoriesRequest, MemoryApp, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};
use crate::error::Error;
use crate::model::{Category, MemorySummary, SessionLogSummary};

#[derive(Clone)]
pub struct ApiState {
    pub app: MemoryApp,
    pub bearer_token: Option<String>,
}

#[derive(Deserialize)]
struct ListMemoriesQuery {
    category: Option<Category>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct RecallPath {
    project: String,
}

#[derive(Deserialize)]
struct MemoryPath {
    id: Uuid,
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
pub struct DeleteEnvelope {
    pub deleted: bool,
    pub id: Uuid,
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
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub id: Uuid,
    pub project: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SessionLogDto {
    pub content: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub cwd: String,
    pub id: Uuid,
    pub project: String,
    pub session_id: String,
    pub summary: String,
}

#[derive(Serialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Serialize)]
struct ApiErrorBody {
    code: &'static str,
    message: String,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/memories", post(create_memory))
        .route("/api/v1/memories/search", post(search_memories))
        .route(
            "/api/v1/memories/{id}",
            get(get_memory).patch(update_memory).delete(delete_memory),
        )
        .route("/api/v1/projects/{project}/memories", get(list_memories))
        .route("/api/v1/projects/{project}/recall", get(recall_project))
        .route("/api/v1/sessions", post(store_session_log))
        .with_state(state)
}

async fn health(State(state): State<ApiState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: state.app.version(),
    })
}

async fn create_memory(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<StoreMemoryRequest>,
) -> Result<Json<MemoryEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memory = state.app.store_memory(request).await?;
    Ok(Json(MemoryEnvelope {
        memory: memory.into(),
    }))
}

async fn search_memories(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<SearchMemoriesRequest>,
) -> Result<Json<SearchEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let response = match state.app.search_memories(request).await? {
        SearchOutcome::Memories(memories) => SearchEnvelope {
            fallback: false,
            memories: memories
                .into_iter()
                .map(|(memory, similarity)| MemoryMatchDto {
                    memory: memory.into(),
                    similarity,
                })
                .collect(),
            session_logs: Vec::new(),
        },
        SearchOutcome::SessionLogs(session_logs) => SearchEnvelope {
            fallback: true,
            memories: Vec::new(),
            session_logs: session_logs
                .into_iter()
                .map(|(session_log, similarity)| SessionLogMatchDto {
                    session_log: session_log.into(),
                    similarity,
                })
                .collect(),
        },
        SearchOutcome::Empty => SearchEnvelope {
            fallback: false,
            memories: Vec::new(),
            session_logs: Vec::new(),
        },
    };
    Ok(Json(response))
}

async fn get_memory(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<MemoryPath>,
) -> Result<Json<MemoryEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memory = state
        .app
        .get_memory(path.id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Memory {} not found", path.id)))?;
    Ok(Json(MemoryEnvelope {
        memory: memory.into(),
    }))
}

async fn update_memory(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<MemoryPath>,
    Json(request): Json<PatchMemoryRequest>,
) -> Result<Json<MemoryEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let request = UpdateMemoryRequest {
        content: request.content,
        id: path.id,
        summary: request.summary,
        tags: request.tags,
    };
    let memory = state
        .app
        .update_memory(request)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Memory {} not found", path.id)))?;
    Ok(Json(MemoryEnvelope {
        memory: memory.into(),
    }))
}

async fn delete_memory(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<MemoryPath>,
) -> Result<Json<DeleteEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let deleted = state.app.delete_memory(path.id).await?;
    if !deleted {
        return Err(ApiError::not_found(format!("Memory {} not found", path.id)));
    }
    Ok(Json(DeleteEnvelope {
        deleted,
        id: path.id,
    }))
}

async fn recall_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<RecallPath>,
) -> Result<Json<MemoryListEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memories = state.app.recall_project(&path.project).await?;
    Ok(Json(MemoryListEnvelope {
        memories: memories.into_iter().map(Into::into).collect(),
    }))
}

async fn store_session_log(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<StoreSessionLogRequest>,
) -> Result<Json<StoreSessionLogEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let session_id = request.session_id.clone();
    let chunk_count = state.app.store_session_log(request).await?;
    Ok(Json(StoreSessionLogEnvelope {
        chunk_count,
        session_id,
    }))
}

async fn list_memories(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<RecallPath>,
    Query(query): Query<ListMemoriesQuery>,
) -> Result<Json<MemoryListEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memories = state
        .app
        .list_memories(ListMemoriesRequest {
            category: query.category,
            limit: query.limit,
            offset: query.offset,
            project: path.project,
        })
        .await?;
    Ok(Json(MemoryListEnvelope {
        memories: memories.into_iter().map(Into::into).collect(),
    }))
}

fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.bearer_token.as_deref() else {
        return Ok(());
    };
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(ApiError::unauthorized("invalid authorization scheme"));
    };
    if token == expected {
        Ok(())
    } else {
        Err(ApiError::unauthorized("invalid bearer token"))
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

#[derive(Debug)]
struct ApiError {
    code: &'static str,
    message: String,
    status: StatusCode,
}

impl ApiError {
    fn not_found(message: String) -> Self {
        Self {
            code: "memory_not_found",
            message,
            status: StatusCode::NOT_FOUND,
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: "unauthorized",
            message: message.into(),
            status: StatusCode::UNAUTHORIZED,
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::Database(inner) => Self {
                code: "database_error",
                message: inner.to_string(),
                status: StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::Embedding(message) => Self {
                code: "embedding_error",
                message,
                status: StatusCode::BAD_GATEWAY,
            },
            Error::NotFound(message) => Self {
                code: "memory_not_found",
                message,
                status: StatusCode::NOT_FOUND,
            },
            Error::Transport(message) => Self {
                code: "transport_error",
                message,
                status: StatusCode::BAD_GATEWAY,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorEnvelope {
                error: ApiErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}
