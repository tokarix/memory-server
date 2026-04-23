use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::MemoryApp;
use crate::error::Error;
use crate::model::Category;
use crate::protocol::{
    AppendSessionMessageDto, AppendSessionMessageRequest, BootstrapEnvelope, CreateSessionRequest,
    DeleteEnvelope, FinalizeSessionDto, FinalizeSessionEnvelope, FinalizeSessionRequest,
    HealthResponse, ListMemoriesRequest, MemoryEnvelope, MemoryListEnvelope, MemoryMatchDto,
    NeighborDto, NeighborListEnvelope, PatchMemoryRequest, RuleListEnvelope, SearchEnvelope,
    SearchMemoriesRequest, SearchOutcome, SessionEnvelope, SessionLogMatchDto,
    SessionMessageEnvelope, StoreMemoryRequest, StoreSessionLogEnvelope, StoreSessionLogRequest,
    SubmitReviewDto, UpdateMemoryRequest,
};

#[derive(Clone)]
pub struct ApiState {
    pub app: MemoryApp,
    pub bearer_token: Option<String>,
}

/// Deserialize an optional comma-separated string into `Option<Vec<String>>`.
///
/// `serde_urlencoded` (used by `axum::extract::Query`) does not support
/// deserializing repeated keys into a `Vec`. We accept a single
/// comma-separated value instead, e.g. `?tags=lang:rust,phase:planning`.
fn deserialize_comma_tags<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.map(|s| {
        s.split(',')
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect()
    }))
}

#[derive(Deserialize)]
struct ListMemoriesQuery {
    category: Option<Category>,
    limit: Option<i64>,
    offset: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_comma_tags")]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RulesQuery {
    include_general: Option<bool>,
    shadow_general: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_comma_tags")]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BootstrapQuery {
    include_general: Option<bool>,
    include_recall: Option<bool>,
}

#[derive(Deserialize)]
struct ReviewQueueQuery {
    category: Option<Category>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct RecallPath {
    project: String,
}

#[derive(Deserialize)]
struct MemoryPath {
    id: Uuid,
}

#[derive(Deserialize)]
struct SessionPath {
    id: Uuid,
}

#[derive(Deserialize)]
struct NeighborQuery {
    limit: Option<i64>,
}

#[derive(serde::Serialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(serde::Serialize)]
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
        .route("/api/v1/memories/{id}/neighbors", get(list_neighbors))
        .route("/api/v1/projects/{project}/memories", get(list_memories))
        .route("/api/v1/projects/{project}/recall", get(recall_project))
        .route("/api/v1/projects/{project}/rules", get(list_rules))
        .route(
            "/api/v1/projects/{project}/bootstrap",
            get(project_bootstrap),
        )
        .route("/api/v1/projects/{project}/review-queue", get(review_queue))
        .route("/api/v1/sessions", post(store_session_log))
        .route("/api/v1/sessions/start", post(create_session))
        .route(
            "/api/v1/sessions/{id}/messages",
            post(append_session_message),
        )
        .route("/api/v1/sessions/{id}/finalize", post(finalize_session))
        .route("/api/v1/review", post(submit_review))
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
    LoggedJson(request): LoggedJson<StoreMemoryRequest>,
) -> Result<Json<MemoryEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memory = state.app.store_memory(request).await?;
    Ok(Json(MemoryEnvelope {
        memory: memory.into(),
    }))
}

async fn create_session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    LoggedJson(request): LoggedJson<CreateSessionRequest>,
) -> Result<Json<SessionEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let session = state.app.create_session(request).await?;
    Ok(Json(SessionEnvelope {
        session: session.into(),
    }))
}

async fn append_session_message(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<SessionPath>,
    LoggedJson(request): LoggedJson<AppendSessionMessageDto>,
) -> Result<Json<SessionMessageEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let message = state
        .app
        .append_session_message(AppendSessionMessageRequest {
            agent: request.agent,
            content: request.content,
            kind: request.kind,
            metadata: request.metadata,
            role: request.role,
            session_id: path.id,
        })
        .await?;
    Ok(Json(SessionMessageEnvelope {
        message: message.into(),
    }))
}

async fn finalize_session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<SessionPath>,
    LoggedJson(request): LoggedJson<FinalizeSessionDto>,
) -> Result<Json<FinalizeSessionEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let chunk_count = state
        .app
        .finalize_session(FinalizeSessionRequest {
            session_id: path.id,
            summary: request.summary,
        })
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Session {} not found", path.id)))?;
    Ok(Json(FinalizeSessionEnvelope {
        chunk_count,
        session_id: path.id,
    }))
}

async fn search_memories(
    State(state): State<ApiState>,
    headers: HeaderMap,
    LoggedJson(request): LoggedJson<SearchMemoriesRequest>,
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
    LoggedJson(request): LoggedJson<PatchMemoryRequest>,
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
    LoggedJson(request): LoggedJson<StoreSessionLogRequest>,
) -> Result<Json<StoreSessionLogEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let session_id = request.session_id.clone();
    let chunk_count = state.app.store_session_log(request).await?;
    Ok(Json(StoreSessionLogEnvelope {
        chunk_count,
        session_id,
    }))
}

async fn review_queue(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<RecallPath>,
    Query(query): Query<ReviewQueueQuery>,
) -> Result<Json<MemoryListEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let memories = state
        .app
        .list_review_queue(&path.project, query.category.as_ref(), limit)
        .await?;
    Ok(Json(MemoryListEnvelope {
        memories: memories.into_iter().map(Into::into).collect(),
    }))
}

async fn list_rules(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<RecallPath>,
    Query(query): Query<RulesQuery>,
) -> Result<Json<RuleListEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let rules = state
        .app
        .list_rules(
            &path.project,
            query.include_general.unwrap_or(true),
            query.shadow_general.unwrap_or(true),
            query.tags.as_deref(),
        )
        .await?;
    Ok(Json(rules.into()))
}

async fn project_bootstrap(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<RecallPath>,
    Query(query): Query<BootstrapQuery>,
) -> Result<Json<BootstrapEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let payload = state
        .app
        .bootstrap_project(
            &path.project,
            query.include_general.unwrap_or(true),
            query.include_recall.unwrap_or(true),
        )
        .await?;
    Ok(Json(payload.into()))
}

async fn submit_review(
    State(state): State<ApiState>,
    headers: HeaderMap,
    LoggedJson(request): LoggedJson<SubmitReviewDto>,
) -> Result<Json<MemoryEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let memory = state
        .app
        .submit_review(
            request.memory_id,
            request.project,
            request.reviewer,
            request.verdict,
            request.notes,
        )
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Memory {} not found", request.memory_id)))?;
    Ok(Json(MemoryEnvelope {
        memory: memory.into(),
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
            tags: query.tags,
        })
        .await?;
    Ok(Json(MemoryListEnvelope {
        memories: memories.into_iter().map(Into::into).collect(),
    }))
}

async fn list_neighbors(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<MemoryPath>,
    Query(query): Query<NeighborQuery>,
) -> Result<Json<NeighborListEnvelope>, ApiError> {
    authorize(&state, &headers)?;
    let neighbors = state.app.list_neighbors(path.id, query.limit).await?;
    Ok(Json(NeighborListEnvelope {
        neighbors: neighbors
            .into_iter()
            .map(|(edge, memory)| NeighborDto {
                edge: edge.into(),
                memory: memory.into(),
            })
            .collect(),
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

#[derive(Debug)]
struct ApiError {
    code: &'static str,
    message: String,
    status: StatusCode,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad_request",
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }

    fn not_found(message: String) -> Self {
        Self {
            code: "not_found",
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

/// A [`Json`] wrapper that logs deserialization failures before returning 400.
struct LoggedJson<T>(T);

impl<S, T> FromRequest<S> for LoggedJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                tracing::warn!("JSON rejection: {rejection}");
                Err(ApiError::bad_request(rejection.body_text()))
            }
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::Database(inner) => Self {
                code: "database_error",
                message: inner.clone(),
                status: StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::Embedding(message) => Self {
                code: "embedding_error",
                message,
                status: StatusCode::BAD_GATEWAY,
            },
            Error::NotFound(message) => Self {
                code: "not_found",
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
