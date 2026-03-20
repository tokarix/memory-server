//! Server-rendered HTML views for browsing and searching stored data.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::header::{COOKIE, HeaderMap, LOCATION, SET_COOKIE};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::ApiState;
use crate::app::{ListMemoriesRequest, SearchMemoriesRequest, SearchOutcome};
use crate::error::Error;
use crate::model::{
    Category, MemorySummary, SessionLogSummary, SessionMessageSummary, SessionSummary,
};

const UI_TOKEN_COOKIE: &str = "memoryd_token";
const SEARCH_LIMIT: i64 = 20;
const SECTION_LIMIT: i64 = 20;

#[derive(Deserialize)]
struct ProjectPath {
    project: String,
}

#[derive(Deserialize)]
struct ItemPath {
    id: Uuid,
}

#[derive(Default, Deserialize)]
struct ProjectQuery {
    q: Option<String>,
    category: Option<Category>,
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

#[derive(Template)]
#[template(path = "ui/index.html")]
struct IndexTemplate {
    projects: Vec<ProjectCard>,
}

#[derive(Template)]
#[template(path = "ui/login.html")]
struct LoginTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "ui/project.html")]
struct ProjectTemplate {
    project: String,
    query: String,
    selected_category: String,
    memories: Vec<MemoryCard>,
    session_logs: Vec<SessionLogCard>,
    sessions: Vec<SessionCard>,
    search_results: String,
}

#[derive(Template)]
#[template(path = "ui/partials/search_results.html")]
struct SearchResultsTemplate {
    query: String,
    has_results: bool,
    fallback: bool,
    memories: Vec<SearchMemoryCard>,
    session_logs: Vec<SearchSessionLogCard>,
}

#[derive(Template)]
#[template(path = "ui/memory.html")]
struct MemoryTemplate {
    memory: MemoryDetailView,
}

#[derive(Template)]
#[template(path = "ui/session_log.html")]
struct SessionLogTemplate {
    session_log: SessionLogDetailView,
}

#[derive(Template)]
#[template(path = "ui/session.html")]
struct SessionTemplate {
    session: SessionDetailView,
    messages: Vec<SessionMessageCard>,
}

#[derive(Clone)]
struct ProjectCard {
    name: String,
}

#[derive(Clone)]
struct MemoryCard {
    id: Uuid,
    summary: String,
    category: String,
    updated_at: String,
    tags: String,
    preview: String,
}

#[derive(Clone)]
struct SessionLogCard {
    id: Uuid,
    summary: String,
    created_at: String,
    cwd: String,
    session_id: String,
    preview: String,
}

#[derive(Clone)]
struct SessionCard {
    id: Uuid,
    updated_at: String,
    created_at: String,
    ended_at: String,
    agent: String,
    cwd: String,
    external_session_id: String,
}

#[derive(Clone)]
struct SearchMemoryCard {
    id: Uuid,
    summary: String,
    category: String,
    similarity: String,
    tags: String,
    preview: String,
}

#[derive(Clone)]
struct SearchSessionLogCard {
    id: Uuid,
    summary: String,
    similarity: String,
    created_at: String,
    session_id: String,
    preview: String,
}

#[derive(Clone)]
struct MemoryDetailView {
    id: Uuid,
    project: String,
    category: String,
    created_at: String,
    updated_at: String,
    summary: String,
    tags: String,
    content: String,
}

#[derive(Clone)]
struct SessionLogDetailView {
    id: Uuid,
    project: String,
    created_at: String,
    cwd: String,
    session_id: String,
    summary: String,
    content: String,
}

#[derive(Clone)]
struct SessionDetailView {
    id: Uuid,
    project: String,
    created_at: String,
    updated_at: String,
    ended_at: String,
    agent: String,
    cwd: String,
    external_session_id: String,
}

#[derive(Clone)]
struct SessionMessageCard {
    created_at: String,
    role: String,
    kind: String,
    agent: String,
    metadata: String,
    content: String,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui", get(index))
        .route("/ui/login", get(login_form).post(login))
        .route("/ui/logout", post(logout))
        .route("/ui/projects/{project}", get(project))
        .route("/ui/projects/{project}/search", get(project_search))
        .route("/ui/memories/{id}", get(memory_detail))
        .route("/ui/session-logs/{id}", get(session_log_detail))
        .route("/ui/sessions/{id}", get(session_detail))
        .with_state(state)
}

async fn root() -> Redirect {
    Redirect::to("/ui")
}

async fn index(State(state): State<ApiState>, headers: HeaderMap) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let projects = state
        .app
        .list_projects()
        .await?
        .into_iter()
        .map(|name| ProjectCard { name })
        .collect();
    render_html(&IndexTemplate { projects })
}

async fn login_form(State(state): State<ApiState>) -> Result<Html<String>, UiError> {
    if state.bearer_token.is_none() {
        return Err(UiError::redirect("/ui"));
    }
    render_html(&LoginTemplate { error: None })
}

async fn login(
    State(state): State<ApiState>,
    Form(form): Form<LoginForm>,
) -> Result<Response, UiError> {
    let Some(expected) = state.bearer_token.as_deref() else {
        return Ok(Redirect::to("/ui").into_response());
    };
    if form.token != expected {
        return render_response(&LoginTemplate {
            error: Some("Invalid token".to_owned()),
        });
    }
    Ok((
        [(SET_COOKIE, session_cookie(&form.token))],
        Redirect::to("/ui"),
    )
        .into_response())
}

async fn logout() -> Response {
    (
        [(SET_COOKIE, clear_session_cookie())],
        Redirect::to("/ui/login"),
    )
        .into_response()
}

async fn project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<ProjectPath>,
    Query(query): Query<ProjectQuery>,
) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let search_results = render_search_results(&state, &path.project, &query).await?;
    let memories = state
        .app
        .list_memories(ListMemoriesRequest {
            category: None,
            limit: Some(SECTION_LIMIT),
            offset: Some(0),
            project: path.project.clone(),
        })
        .await?
        .into_iter()
        .map(MemoryCard::from)
        .collect();
    let session_logs = state
        .app
        .list_session_logs(&path.project, Some(SECTION_LIMIT), Some(0))
        .await?
        .into_iter()
        .map(SessionLogCard::from)
        .collect();
    let sessions = state
        .app
        .list_sessions(&path.project, Some(SECTION_LIMIT), Some(0))
        .await?
        .into_iter()
        .map(SessionCard::from)
        .collect();
    render_html(&ProjectTemplate {
        project: path.project,
        query: query.q.unwrap_or_default(),
        selected_category: category_param(query.category.as_ref()),
        memories,
        session_logs,
        sessions,
        search_results,
    })
}

async fn project_search(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<ProjectPath>,
    Query(query): Query<ProjectQuery>,
) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let results = render_search_results(&state, &path.project, &query).await?;
    Ok(Html(results))
}

async fn memory_detail(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<ItemPath>,
) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let memory = state
        .app
        .get_memory(path.id)
        .await?
        .ok_or_else(|| UiError::not_found(format!("Memory {} not found", path.id)))?;
    render_html(&MemoryTemplate {
        memory: MemoryDetailView::from(memory),
    })
}

async fn session_log_detail(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<ItemPath>,
) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let session_log = state
        .app
        .get_session_log(path.id)
        .await?
        .ok_or_else(|| UiError::not_found(format!("Session log {} not found", path.id)))?;
    render_html(&SessionLogTemplate {
        session_log: SessionLogDetailView::from(session_log),
    })
}

async fn session_detail(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(path): Path<ItemPath>,
) -> Result<Html<String>, UiError> {
    authorize_ui(&state, &headers)?;
    let session = state
        .app
        .get_session(path.id)
        .await?
        .ok_or_else(|| UiError::not_found(format!("Session {} not found", path.id)))?;
    let messages = state
        .app
        .list_session_messages(path.id)
        .await?
        .into_iter()
        .map(SessionMessageCard::from)
        .collect();
    render_html(&SessionTemplate {
        session: SessionDetailView::from(session),
        messages,
    })
}

async fn render_search_results(
    state: &ApiState,
    project: &str,
    query: &ProjectQuery,
) -> Result<String, UiError> {
    let trimmed = query.q.as_deref().map(str::trim).unwrap_or_default();
    let template = if trimmed.is_empty() {
        SearchResultsTemplate {
            query: String::new(),
            has_results: false,
            fallback: false,
            memories: Vec::new(),
            session_logs: Vec::new(),
        }
    } else {
        match state
            .app
            .search_memories(SearchMemoriesRequest {
                category: query.category.clone(),
                cross_project: None,
                graph_hops: None,
                include_general: None,
                limit: Some(SEARCH_LIMIT),
                min_similarity: None,
                project: project.to_owned(),
                project_allowlist: None,
                query: trimmed.to_owned(),
            })
            .await?
        {
            SearchOutcome::Memories(memories) => SearchResultsTemplate {
                query: trimmed.to_owned(),
                has_results: !memories.is_empty(),
                fallback: false,
                memories: memories
                    .into_iter()
                    .map(|(memory, similarity)| SearchMemoryCard::from_match(memory, similarity))
                    .collect(),
                session_logs: Vec::new(),
            },
            SearchOutcome::SessionLogs(session_logs) => SearchResultsTemplate {
                query: trimmed.to_owned(),
                has_results: !session_logs.is_empty(),
                fallback: true,
                memories: Vec::new(),
                session_logs: session_logs
                    .into_iter()
                    .map(|(session_log, similarity)| {
                        SearchSessionLogCard::from_match(session_log, similarity)
                    })
                    .collect(),
            },
            SearchOutcome::Empty => SearchResultsTemplate {
                query: trimmed.to_owned(),
                has_results: false,
                fallback: false,
                memories: Vec::new(),
                session_logs: Vec::new(),
            },
        }
    };
    template
        .render()
        .map_err(|error| UiError::internal(error.to_string()))
}

fn authorize_ui(state: &ApiState, headers: &HeaderMap) -> Result<(), UiError> {
    let Some(expected) = state.bearer_token.as_deref() else {
        return Ok(());
    };
    if bearer_token(headers).is_some_and(|token| token == expected) {
        return Ok(());
    }
    if cookie_value(headers, UI_TOKEN_COOKIE).is_some_and(|token| token == expected) {
        return Ok(());
    }
    Err(UiError::redirect("/ui/login"))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(COOKIE)
        .and_then(|header| header.to_str().ok())
        .and_then(|cookie_header| {
            cookie_header.split(';').find_map(|entry| {
                let trimmed = entry.trim();
                let (key, value) = trimmed.split_once('=')?;
                (key == name).then_some(value)
            })
        })
}

fn session_cookie(token: &str) -> String {
    format!("{UI_TOKEN_COOKIE}={token}; HttpOnly; Path=/; SameSite=Lax")
}

fn clear_session_cookie() -> String {
    format!("{UI_TOKEN_COOKIE}=; HttpOnly; Max-Age=0; Path=/; SameSite=Lax")
}

fn category_param(category: Option<&Category>) -> String {
    category.map_or_else(String::new, ToString::to_string)
}

fn render_html<T: Template>(template: &T) -> Result<Html<String>, UiError> {
    let rendered = template
        .render()
        .map_err(|error| UiError::internal(error.to_string()))?;
    Ok(Html(rendered))
}

fn render_response<T: Template>(template: &T) -> Result<Response, UiError> {
    let rendered = template
        .render()
        .map_err(|error| UiError::internal(error.to_string()))?;
    Ok(Html(rendered).into_response())
}

fn format_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn optional_timestamp(timestamp: Option<chrono::DateTime<chrono::Utc>>) -> String {
    timestamp.map_or_else(|| "-".to_owned(), format_timestamp)
}

fn preview(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}...")
}

fn tags_line(tags: &[String]) -> String {
    if tags.is_empty() {
        "-".to_owned()
    } else {
        tags.join(", ")
    }
}

fn similarity_label(similarity: f64) -> String {
    format!("{similarity:.3}")
}

impl From<MemorySummary> for MemoryCard {
    fn from(memory: MemorySummary) -> Self {
        Self {
            id: memory.id,
            summary: memory.summary,
            category: memory.category.to_string(),
            updated_at: format_timestamp(memory.updated_at),
            tags: tags_line(&memory.tags),
            preview: preview(&memory.content, 180),
        }
    }
}

impl From<SessionLogSummary> for SessionLogCard {
    fn from(session_log: SessionLogSummary) -> Self {
        Self {
            id: session_log.id,
            summary: session_log.summary,
            created_at: format_timestamp(session_log.created_at),
            cwd: session_log.cwd,
            session_id: session_log.session_id,
            preview: preview(&session_log.content, 180),
        }
    }
}

impl From<SessionSummary> for SessionCard {
    fn from(session: SessionSummary) -> Self {
        Self {
            id: session.id,
            updated_at: format_timestamp(session.updated_at),
            created_at: format_timestamp(session.created_at),
            ended_at: optional_timestamp(session.ended_at),
            agent: if session.agent.is_empty() {
                "-".to_owned()
            } else {
                session.agent
            },
            cwd: session.cwd,
            external_session_id: session.external_session_id,
        }
    }
}

impl SearchMemoryCard {
    fn from_match(memory: MemorySummary, similarity: f64) -> Self {
        Self {
            id: memory.id,
            summary: memory.summary,
            category: memory.category.to_string(),
            similarity: similarity_label(similarity),
            tags: tags_line(&memory.tags),
            preview: preview(&memory.content, 200),
        }
    }
}

impl SearchSessionLogCard {
    fn from_match(session_log: SessionLogSummary, similarity: f64) -> Self {
        Self {
            id: session_log.id,
            summary: session_log.summary,
            similarity: similarity_label(similarity),
            created_at: format_timestamp(session_log.created_at),
            session_id: session_log.session_id,
            preview: preview(&session_log.content, 200),
        }
    }
}

impl From<MemorySummary> for MemoryDetailView {
    fn from(memory: MemorySummary) -> Self {
        Self {
            id: memory.id,
            project: memory.project,
            category: memory.category.to_string(),
            created_at: format_timestamp(memory.created_at),
            updated_at: format_timestamp(memory.updated_at),
            summary: memory.summary,
            tags: tags_line(&memory.tags),
            content: memory.content,
        }
    }
}

impl From<SessionLogSummary> for SessionLogDetailView {
    fn from(session_log: SessionLogSummary) -> Self {
        Self {
            id: session_log.id,
            project: session_log.project,
            created_at: format_timestamp(session_log.created_at),
            cwd: session_log.cwd,
            session_id: session_log.session_id,
            summary: session_log.summary,
            content: session_log.content,
        }
    }
}

impl From<SessionSummary> for SessionDetailView {
    fn from(session: SessionSummary) -> Self {
        Self {
            id: session.id,
            project: session.project,
            created_at: format_timestamp(session.created_at),
            updated_at: format_timestamp(session.updated_at),
            ended_at: optional_timestamp(session.ended_at),
            agent: if session.agent.is_empty() {
                "-".to_owned()
            } else {
                session.agent
            },
            cwd: session.cwd,
            external_session_id: session.external_session_id,
        }
    }
}

impl From<SessionMessageSummary> for SessionMessageCard {
    fn from(message: SessionMessageSummary) -> Self {
        Self {
            created_at: format_timestamp(message.created_at),
            role: message.role,
            kind: message.kind,
            agent: if message.agent.is_empty() {
                "-".to_owned()
            } else {
                message.agent
            },
            metadata: message.metadata.unwrap_or_else(|| "-".to_owned()),
            content: message.content,
        }
    }
}

#[derive(Debug)]
struct UiError {
    message: String,
    redirect_to: Option<String>,
    status: axum::http::StatusCode,
}

impl UiError {
    fn internal(message: String) -> Self {
        Self {
            message,
            redirect_to: None,
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn not_found(message: String) -> Self {
        Self {
            message,
            redirect_to: None,
            status: axum::http::StatusCode::NOT_FOUND,
        }
    }

    fn redirect(path: &str) -> Self {
        Self {
            message: String::new(),
            redirect_to: Some(path.to_owned()),
            status: axum::http::StatusCode::SEE_OTHER,
        }
    }
}

impl From<Error> for UiError {
    fn from(error: Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        if let Some(path) = self.redirect_to {
            return (self.status, [(LOCATION, path)]).into_response();
        }
        (self.status, Html(self.message)).into_response()
    }
}
