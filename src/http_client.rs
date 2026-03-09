use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::api::{
    AppendSessionMessageDto, BootstrapEnvelope, CreateSessionDto, DeleteEnvelope,
    FinalizeSessionDto, FinalizeSessionEnvelope, HealthResponse, MemoryEnvelope,
    MemoryListEnvelope, PatchMemoryRequest, RuleListEnvelope, SearchEnvelope, SessionEnvelope,
    SessionMessageEnvelope, StoreSessionLogEnvelope, SubmitPlanReviewDto,
};
use crate::app::{
    AppendSessionMessageRequest, BootstrapPayload, CreateSessionRequest, FinalizeSessionRequest,
    ListMemoriesRequest, RuleList, SearchMemoriesRequest, SearchOutcome, StoreMemoryRequest,
    StoreSessionLogRequest, UpdateMemoryRequest,
};
use crate::error::Error;
use crate::model;

#[derive(Clone)]
pub struct HttpMemoryClient {
    base_url: reqwest::Url,
    bearer_token: Option<String>,
    http: reqwest::Client,
}

impl HttpMemoryClient {
    pub fn new(base_url: &str, bearer_token: Option<String>) -> Result<Self, Error> {
        let base_url = reqwest::Url::parse(base_url)
            .map_err(|error| Error::Transport(format!("invalid memoryd_url: {error}")))?;
        Ok(Self {
            base_url,
            bearer_token,
            http: reqwest::Client::new(),
        })
    }

    pub async fn version(&self) -> Result<String, Error> {
        let response: HealthResponse = self.request(Method::GET, &["api", "v1", "health"]).await?;
        Ok(response.version)
    }

    pub async fn delete_memory(&self, id: Uuid) -> Result<bool, Error> {
        let id = id.to_string();
        let response: DeleteEnvelope = self
            .request(Method::DELETE, &["api", "v1", "memories", &id])
            .await?;
        Ok(response.deleted)
    }

    pub async fn get_memory(&self, id: Uuid) -> Result<Option<model::MemorySummary>, Error> {
        let id = id.to_string();
        match self
            .request::<MemoryEnvelope>(Method::GET, &["api", "v1", "memories", &id])
            .await
        {
            Ok(response) => Ok(Some(response.memory.into())),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn list_memories(
        &self,
        request: ListMemoriesRequest,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        let mut url = self.url(&["api", "v1", "projects", &request.project, "memories"])?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(category) = request.category {
                query.append_pair("category", &category.to_string());
            }
            if let Some(limit) = request.limit {
                query.append_pair("limit", &limit.to_string());
            }
            if let Some(offset) = request.offset {
                query.append_pair("offset", &offset.to_string());
            }
        }
        let response: MemoryListEnvelope = self.request_url(Method::GET, url, None::<&()>).await?;
        Ok(response.memories.into_iter().map(Into::into).collect())
    }

    pub async fn recall_project(&self, project: &str) -> Result<Vec<model::MemorySummary>, Error> {
        let response: MemoryListEnvelope = self
            .request(Method::GET, &["api", "v1", "projects", project, "recall"])
            .await?;
        Ok(response.memories.into_iter().map(Into::into).collect())
    }

    pub async fn list_rules(
        &self,
        project: &str,
        include_general: bool,
    ) -> Result<RuleList, Error> {
        let mut url = self.url(&["api", "v1", "projects", project, "rules"])?;
        url.query_pairs_mut()
            .append_pair("include_general", &include_general.to_string());
        let response: RuleListEnvelope = self.request_url(Method::GET, url, None::<&()>).await?;
        Ok(RuleList {
            general_rules: response.general_rules.into_iter().map(Into::into).collect(),
            project_rules: response.project_rules.into_iter().map(Into::into).collect(),
        })
    }

    pub async fn bootstrap_project(
        &self,
        project: &str,
        include_general: bool,
        include_recall: bool,
    ) -> Result<BootstrapPayload, Error> {
        let mut url = self.url(&["api", "v1", "projects", project, "bootstrap"])?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("include_general", &include_general.to_string());
            query.append_pair("include_recall", &include_recall.to_string());
        }
        let response: BootstrapEnvelope = self.request_url(Method::GET, url, None::<&()>).await?;
        Ok(BootstrapPayload {
            general_rules: response.general_rules.into_iter().map(Into::into).collect(),
            project: response.project,
            project_rules: response.project_rules.into_iter().map(Into::into).collect(),
            recall_memories: response
                .recall_memories
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    pub async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<model::SessionSummary, Error> {
        let response: SessionEnvelope = self
            .request_with_body(
                Method::POST,
                &["api", "v1", "sessions", "start"],
                &CreateSessionDto {
                    agent: request.agent,
                    cwd: request.cwd,
                    external_session_id: request.external_session_id,
                    project: request.project,
                },
            )
            .await?;
        Ok(response.session.into())
    }

    pub async fn append_session_message(
        &self,
        request: AppendSessionMessageRequest,
    ) -> Result<model::SessionMessageSummary, Error> {
        let session_id = request.session_id.to_string();
        let response: SessionMessageEnvelope = self
            .request_with_body(
                Method::POST,
                &["api", "v1", "sessions", &session_id, "messages"],
                &AppendSessionMessageDto {
                    agent: request.agent,
                    content: request.content,
                    kind: request.kind,
                    metadata: request.metadata,
                    role: request.role,
                },
            )
            .await?;
        Ok(response.message.into())
    }

    pub async fn finalize_session(
        &self,
        request: FinalizeSessionRequest,
    ) -> Result<Option<usize>, Error> {
        let session_id = request.session_id.to_string();
        match self
            .request_with_body::<_, FinalizeSessionEnvelope>(
                Method::POST,
                &["api", "v1", "sessions", &session_id, "finalize"],
                &FinalizeSessionDto {
                    summary: request.summary,
                },
            )
            .await
        {
            Ok(response) => Ok(Some(response.chunk_count)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn list_plan_review_queue(
        &self,
        project: &str,
        limit: i64,
    ) -> Result<Vec<model::MemorySummary>, Error> {
        let mut url = self.url(&["api", "v1", "projects", project, "plans", "review-queue"])?;
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        let response: MemoryListEnvelope = self.request_url(Method::GET, url, None::<&()>).await?;
        Ok(response.memories.into_iter().map(Into::into).collect())
    }

    pub async fn submit_plan_review(
        &self,
        plan_id: Uuid,
        project: Option<String>,
        reviewer: String,
        verdict: String,
        notes: String,
    ) -> Result<Option<model::MemorySummary>, Error> {
        match self
            .request_with_body::<_, MemoryEnvelope>(
                Method::POST,
                &["api", "v1", "plans", "review"],
                &SubmitPlanReviewDto {
                    notes,
                    plan_id,
                    project,
                    reviewer,
                    verdict,
                },
            )
            .await
        {
            Ok(response) => Ok(Some(response.memory.into())),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn search_memories(
        &self,
        request: SearchMemoriesRequest,
    ) -> Result<SearchOutcome, Error> {
        let response: SearchEnvelope = self
            .request_with_body(Method::POST, &["api", "v1", "memories", "search"], &request)
            .await?;

        if !response.memories.is_empty() {
            return Ok(SearchOutcome::Memories(
                response
                    .memories
                    .into_iter()
                    .map(|entry| (entry.memory.into(), entry.similarity))
                    .collect(),
            ));
        }

        if !response.session_logs.is_empty() {
            return Ok(SearchOutcome::SessionLogs(
                response
                    .session_logs
                    .into_iter()
                    .map(|entry| (entry.session_log.into(), entry.similarity))
                    .collect(),
            ));
        }

        Ok(SearchOutcome::Empty)
    }

    pub async fn store_memory(
        &self,
        request: StoreMemoryRequest,
    ) -> Result<model::MemorySummary, Error> {
        let response: MemoryEnvelope = self
            .request_with_body(Method::POST, &["api", "v1", "memories"], &request)
            .await?;
        Ok(response.memory.into())
    }

    pub async fn update_memory(
        &self,
        request: UpdateMemoryRequest,
    ) -> Result<Option<model::MemorySummary>, Error> {
        let id = request.id.to_string();
        let body = PatchMemoryRequest {
            content: request.content,
            summary: request.summary,
            tags: request.tags,
        };
        match self
            .request_with_body::<_, MemoryEnvelope>(
                Method::PATCH,
                &["api", "v1", "memories", &id],
                &body,
            )
            .await
        {
            Ok(response) => Ok(Some(response.memory.into())),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn store_session_log(&self, request: StoreSessionLogRequest) -> Result<usize, Error> {
        let response: StoreSessionLogEnvelope = self
            .request_with_body(Method::POST, &["api", "v1", "sessions"], &request)
            .await?;
        Ok(response.chunk_count)
    }

    async fn request<T>(&self, method: Method, segments: &[&str]) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        self.request_url(method, self.url(segments)?, None::<&()>)
            .await
    }

    async fn request_with_body<B, T>(
        &self,
        method: Method,
        segments: &[&str],
        body: &B,
    ) -> Result<T, Error>
    where
        B: serde::Serialize + ?Sized,
        T: DeserializeOwned,
    {
        self.request_url(method, self.url(segments)?, Some(body))
            .await
    }

    async fn request_url<B, T>(
        &self,
        method: Method,
        url: reqwest::Url,
        body: Option<&B>,
    ) -> Result<T, Error>
    where
        B: serde::Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let mut request = self.http.request(method, url);
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;

        if response.status().is_success() {
            return response
                .json()
                .await
                .map_err(|error| Error::Transport(format!("failed to parse response: {error}")));
        }

        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable>".to_owned());
        match status {
            StatusCode::NOT_FOUND => Err(Error::NotFound(body)),
            _ => Err(Error::Transport(body)),
        }
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url, Error> {
        let mut url = self.base_url.clone();
        {
            let mut path_segments = url
                .path_segments_mut()
                .map_err(|_| Error::Transport("memoryd_url cannot be a base URL".to_owned()))?;
            path_segments.clear();
            path_segments.extend(segments);
        }
        Ok(url)
    }
}
