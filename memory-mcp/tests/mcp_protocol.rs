//! Black-box stdio contracts captured with rmcp 1.5.0 and a synthetic HTTP peer.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const ID: &str = "00000000-0000-0000-0000-000000000001";
const MISSING: &str = "00000000-0000-0000-0000-000000000002";
const DEADLINE: Duration = Duration::from_secs(10);
const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

#[derive(Clone, Default)]
struct Backend {
    memory: Arc<Mutex<Option<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    guardrail_override: Arc<Mutex<Option<Value>>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

fn memory() -> Value {
    json!({"id":ID,"project":"fixture","category":"decision","summary":"Choice",
        "content":"Use a synthetic backend.","tags":["review-needed"],
        "created_at":"2025-06-15T12:00:00Z","updated_at":"2025-06-15T12:00:27.123456Z"})
}

fn rule(id: &str, project: &str, summary: &str) -> Value {
    json!({"id":id,"project":project,"category":"rule","summary":summary,
        "content":summary,"tags":["lang:rust"],
        "created_at":"2025-06-15T12:00:00Z","updated_at":"2025-06-15T12:00:27.123456Z"})
}

fn fixture_guardrails() -> Value {
    fixture_guardrails_revision(1)
}

fn fixture_guardrails_revision(revision: i64) -> Value {
    use memory_common::guardrails::GuardrailPack;
    use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors, ResolutionContext};

    serde_json::to_value(
        GuardrailPack::new(
            "fixture".to_owned(),
            ResolutionContext::default(),
            1,
            vec![CanonicalRule {
                project: "general".to_owned(),
                id: uuid::Uuid::from_u128(10),
                policy_key: Some("fixture.mandatory".to_owned()),
                revision: Some(revision),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: std::collections::BTreeMap::default(),
                content: "Use the exact mandatory fixture policy.\nKeep its text intact."
                    .to_owned(),
                overrides: None,
            }],
        )
        .unwrap(),
    )
    .unwrap()
}

fn startup_request() -> Value {
    json!({"method":"GET","uri":"/api/v1/projects/fixture/guardrails?context=%7B%7D","body":null})
}

async fn guardrail_response(state: &Backend, uri: &Uri) -> Option<Response> {
    if uri.path() != "/api/v1/projects/fixture/guardrails" {
        return None;
    }
    let override_value = state.guardrail_override.lock().unwrap().clone();
    if let Some(override_value) = override_value {
        if override_value.is_null() {
            return Some(StatusCode::NOT_FOUND.into_response());
        }
        if override_value == "unauthorized" {
            return Some(StatusCode::UNAUTHORIZED.into_response());
        }
        if override_value == "failure" {
            return Some(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
        if override_value == "timeout" {
            tokio::time::sleep(Duration::from_secs(6)).await;
            return Some(Json(fixture_guardrails()).into_response());
        }
        return Some(Json(override_value).into_response());
    }
    Some(Json(fixture_guardrails()).into_response())
}

async fn backend(State(state): State<Backend>, method: Method, uri: Uri, bytes: Bytes) -> Response {
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    state
        .requests
        .lock()
        .unwrap()
        .push(json!({"method":method.as_str(),"uri":uri.to_string(),"body":body}));
    if let Some(response) = guardrail_response(&state, &uri).await {
        return response;
    }
    if uri.path().contains("/hold/") {
        state.entered.notify_one();
        state.release.notified().await;
        return Json(json!({"memories":[]})).into_response();
    }
    if uri.path().contains("/failure/") {
        return (StatusCode::BAD_GATEWAY, "fixture unavailable").into_response();
    }
    let mut stored = state.memory.lock().unwrap();
    let result = match (method.as_str(), uri.path()) {
        ("GET", "/api/v1/health") => json!({"status":"ok","version":"fixture-version"}),
        ("POST", "/api/v1/memories") => {
            let mut value = memory();
            for key in ["project", "category", "summary", "content", "tags"] {
                if !body[key].is_null() {
                    value[key] = body[key].clone();
                }
            }
            if !body["policy"].is_null() {
                value["policy"] = body["policy"].clone();
                value["policy"]["state"] = json!("active");
            }
            *stored = Some(value.clone());
            json!({"memory":value})
        }
        ("POST", "/api/v1/memories/search") => match body["query"].as_str().unwrap() {
            "hit" => {
                json!({"fallback":false,"memories":[{"memory":memory(),"similarity":0.875}],"session_logs":[]})
            }
            "fallback" => {
                json!({"fallback":true,"memories":[],"session_logs":[{"similarity":0.75,"session_log":{
                "id":ID,"content":"Synthetic transcript","created_at":"2025-06-15T12:00:00Z",
                "cwd":"/fixture","project":"fixture","session_id":"session","summary":"Transcript"}}]})
            }
            _ => json!({"fallback":false,"memories":[],"session_logs":[]}),
        },
        ("POST", "/api/v1/review") => {
            if body["memory_id"] == MISSING {
                return StatusCode::NOT_FOUND.into_response();
            }
            json!({"memory":memory()})
        }
        ("GET", "/api/v1/projects/cockpit/rules") => json!({
            "project_rules":[rule(ID,"cockpit","Rust rules loaded")],
            "general_rules":[
                rule("00000000-0000-0000-0000-000000000003","general","Keep Rust build artifacts off tmpfs"),
                rule("00000000-0000-0000-0000-000000000004","general","Run Rust verification checks")
            ]
        }),
        ("GET", path) if path.ends_with("/rules") => json!({"general_rules":[],"project_rules":[]}),
        ("GET", path) if path.ends_with("/bootstrap") => {
            json!({"project":"fixture","general_rules":[],"project_rules":[],"recall_memories":[]})
        }
        ("GET", path) if path.ends_with("/neighbors") => json!({"neighbors":[]}),
        ("GET", path) if path.ends_with("/review-queue") => {
            json!({"memories":stored.iter().collect::<Vec<_>>()})
        }
        ("GET", path) if path.ends_with("/memories") || path.ends_with("/recall") => {
            json!({"memories":stored.iter().collect::<Vec<_>>()})
        }
        ("DELETE", path) => {
            let deleted = path.ends_with(ID) && stored.take().is_some();
            json!({"id":if path.ends_with(ID) { ID } else { MISSING },"deleted":deleted})
        }
        ("GET" | "PATCH", path) if path.ends_with(ID) && stored.is_some() => {
            let value = stored.as_mut().unwrap();
            if method == Method::PATCH {
                for key in ["summary", "content", "tags"] {
                    if !body[key].is_null() {
                        value[key] = body[key].clone();
                    }
                }
                if !body["policy"].is_null() {
                    value["policy"] = body["policy"].clone();
                    value["policy"]["state"] = json!("active");
                }
            }
            json!({"memory":value})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(result).into_response()
}

struct Fixture {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: JoinHandle<String>,
    config: PathBuf,
    state: Backend,
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
    transcript: Vec<Value>,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with_state(Backend::default()).await
    }

    async fn start_with_state(state: Backend) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let app = Router::new().fallback(backend).with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self::start_on(&format!("http://{address}"), state, stop, server)
    }

    fn start_real(url: &str) -> Self {
        Self::start_real_with_context(url, None)
    }

    fn start_real_with_context(url: &str, context: Option<&str>) -> Self {
        let state = Backend::default();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = stopped.await;
        });
        Self::start_on_with_context(url, state, stop, server, context)
    }

    fn start_on(
        url: &str,
        state: Backend,
        stop: oneshot::Sender<()>,
        server: JoinHandle<()>,
    ) -> Self {
        Self::start_on_with_context(url, state, stop, server, None)
    }

    fn start_on_with_context(
        url: &str,
        state: Backend,
        stop: oneshot::Sender<()>,
        server: JoinHandle<()>,
        context: Option<&str>,
    ) -> Self {
        let config =
            std::env::temp_dir().join(format!("mcp-contract-{}.toml", uuid::Uuid::new_v4()));
        let context_line = context.map_or_else(String::new, |value| {
            format!("resolution_context = {value}\n")
        });
        std::fs::write(
            &config,
            format!("memoryd_url = \"{url}\"\nguardrails_project = \"fixture\"\n{context_line}"),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_memory-mcp"))
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            err.read_to_string(&mut text).await.unwrap();
            text
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
            config,
            state,
            stop,
            server,
            transcript: vec![],
        }
    }

    async fn send(&mut self, request: &Value) {
        let mut bytes = serde_json::to_vec(request).unwrap();
        bytes.push(b'\n');
        self.stdin
            .as_mut()
            .unwrap()
            .write_all(&bytes)
            .await
            .unwrap();
    }

    async fn reconnect(&mut self, version: &str) {
        self.stdin.take();
        timeout(DEADLINE, self.child.wait()).await.unwrap().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_memory-mcp"))
            .arg(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            err.read_to_string(&mut text).await.unwrap();
            text
        });
        let previous = std::mem::replace(&mut self.stderr, stderr);
        previous.await.unwrap();
        self.child = child;
        self.stdin = stdin;
        self.stdout = stdout;
        self.initialize(version).await;
    }

    async fn expect_startup_failure(mut self, code: &str) {
        let status = timeout(DEADLINE, self.child.wait()).await.unwrap().unwrap();
        assert!(!status.success());
        assert!(self.stdout.next_line().await.unwrap().is_none());
        let stderr = self.stderr.await.unwrap();
        assert!(stderr.contains(code), "{stderr}");
        self.stop.send(()).unwrap();
        timeout(DEADLINE, self.server).await.unwrap().unwrap();
        std::fs::remove_file(&self.config).unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = format!("request-{}", self.transcript.len());
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.send(&request).await;
        let line = timeout(DEADLINE, self.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("JSON-RPC response");
        let mut response: Value =
            serde_json::from_str(&line).expect("stdout must contain only JSON-RPC");
        assert_eq!(response["id"], id);
        assert_eq!(response["jsonrpc"], "2.0");
        if method == "initialize" {
            assert_eq!(
                response["result"]["serverInfo"]["version"],
                concat!(env!("CARGO_PKG_VERSION"), "-", env!("GIT_HASH"))
            );
            response["result"]["serverInfo"]["version"] = json!("<package-version>-<git-hash>");
        }
        self.transcript
            .push(json!({"request":request,"response":response}));
        response
    }

    async fn initialize(&mut self, version: &str) {
        let response = self.request("initialize", json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"contract","version":"1"}})).await;
        assert_eq!(
            response["result"]["protocolVersion"],
            if VERSIONS.contains(&version) {
                version
            } else {
                VERSIONS[3]
            }
        );
        assert_eq!(response["result"]["capabilities"], json!({"tools":{}}));
        self.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }

    async fn finish(mut self, success: bool) -> Value {
        drop(self.stdin.take());
        let status = timeout(DEADLINE, self.child.wait())
            .await
            .expect("bounded EOF exit")
            .unwrap();
        assert_eq!(status.success(), success);
        let mut remaining = vec![];
        while let Some(line) = self.stdout.next_line().await.unwrap() {
            remaining.push(serde_json::from_str::<Value>(&line).expect("stdout JSON-RPC only"));
        }
        let stderr = self.stderr.await.unwrap();
        assert!(stderr.contains("starting MCP stdio server"));
        self.state.release.notify_one();
        self.stop.send(()).unwrap();
        timeout(DEADLINE, self.server).await.unwrap().unwrap();
        std::fs::remove_file(&self.config).unwrap();
        json!({"transcript":self.transcript,"http":*self.state.requests.lock().unwrap(),"after_eof":remaining})
    }
}

fn golden(name: &str, actual: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"));
    if std::env::var_os("UPDATE_MCP_GOLDENS").is_some() {
        let mut bytes = serde_json::to_vec_pretty(actual).unwrap();
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(*actual, expected, "rmcp 1.5 wire contract: {name}");
}

fn tool_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

fn metadata_header<'a>(text: &'a str, name: &str) -> &'a str {
    text.split_once("\n\n")
        .unwrap()
        .0
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap()
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn trusted_mcp_context_reaches_real_policy_resolution(pool: sqlx::PgPool) {
    use chrono::Utc;
    use memory_common::policy::{DeliveryClass, PolicySelectors, PolicyWrite};
    use memoryd::api::{ApiState, router};
    use memoryd::app::MemoryApp;
    use memoryd::embed;
    use memoryd::model::{Category, Memory};

    for (id, key, profile) in [
        (101, "storage.host", "workstation-host"),
        (102, "storage.ci", "woodpecker-container"),
    ] {
        let now = Utc::now();
        let memory = Memory {
            id: uuid::Uuid::from_u128(id),
            policy: None,
            category: Category::Rule,
            content: format!("Required {profile} storage"),
            created_at: now,
            embedding: vec![0.0; 1024],
            project: "general".to_owned(),
            summary: format!("{profile} storage"),
            tags: vec!["storage".to_owned()],
            updated_at: now,
        };
        let policy = PolicyWrite {
            policy_key: key.to_owned(),
            revision: 1,
            delivery_class: DeliveryClass::Mandatory,
            supersedes: None,
            selectors: PolicySelectors {
                profile: Some([profile.to_owned()].into()),
                language: Some(["rust".to_owned()].into()),
                ..PolicySelectors::default()
            },
            values: std::collections::BTreeMap::new(),
        };
        memoryd::policy::publish_new(&pool, &memory, &policy)
            .await
            .unwrap();
    }
    let url = "http://127.0.0.1:1".to_owned();
    let app = MemoryApp::new(
        pool.clone(),
        Arc::new(embed::Client::new(
            url.clone(),
            "unused".to_owned(),
            None,
            None,
        )),
        "unused".to_owned(),
        1024,
        reqwest::Client::new(),
        url,
        "unused".to_owned(),
        1024,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(ApiState {
                app,
                bearer_token: None,
            }),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    let api_url = format!("http://{address}");
    for (profile, expected, excluded) in [
        (
            "workstation-host",
            "Required workstation-host storage",
            "Required woodpecker-container storage",
        ),
        (
            "woodpecker-container",
            "Required woodpecker-container storage",
            "Required workstation-host storage",
        ),
    ] {
        let mut fixture = Fixture::start_real_with_context(
            &api_url,
            Some(&format!(
                "{{ profile = \"{profile}\", language = [\"rust\"] }}"
            )),
        );
        fixture.initialize(VERSIONS[3]).await;
        for tool in ["memory_rules", "memory_bootstrap"] {
            let response = fixture
                .call(
                    tool,
                    json!({"project":"app","include_general":false,
                "tags":["missing"],"include_recall":false}),
                )
                .await;
            let text = tool_text(&response);
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains(excluded), "{text}");
        }
        let rejected = fixture
            .call(
                "memory_rules",
                json!({"project":"app",
            "context":{"profile":"different"}}),
            )
            .await;
        assert_eq!(rejected["error"]["data"]["code"], "policy_context_mismatch");
        if profile == "workstation-host" {
            let original = fixture.call("memory_guardrails", json!({})).await;
            let original: Value = serde_json::from_str(tool_text(&original)).unwrap();
            let now = Utc::now();
            let successor = Memory {
                id: uuid::Uuid::from_u128(103),
                policy: None,
                category: Category::Rule,
                content: "Updated required host storage".to_owned(),
                created_at: now,
                embedding: vec![0.0; 1024],
                project: "general".to_owned(),
                summary: "Updated host storage".to_owned(),
                tags: vec!["storage".to_owned()],
                updated_at: now,
            };
            let policy = PolicyWrite {
                policy_key: "storage.host".to_owned(),
                revision: 2,
                delivery_class: DeliveryClass::Mandatory,
                supersedes: Some(uuid::Uuid::from_u128(101)),
                selectors: PolicySelectors {
                    profile: Some(["workstation-host".to_owned()].into()),
                    language: Some(["rust".to_owned()].into()),
                    ..PolicySelectors::default()
                },
                values: std::collections::BTreeMap::new(),
            };
            memoryd::policy::publish_new(&pool, &successor, &policy)
                .await
                .unwrap();
            let stale = fixture.call("memory_guardrails", json!({})).await;
            assert_eq!(stale["error"]["data"]["code"], "guardrails_changed");
            assert_eq!(
                stale["error"]["data"]["details"]["old_digest"],
                original["digest"]
            );
            let stale_list = fixture.request("tools/list", json!({})).await;
            assert_eq!(stale_list["error"]["data"]["code"], "guardrails_changed");
            fixture.reconnect(VERSIONS[3]).await;
            let fresh = fixture.call("memory_guardrails", json!({})).await;
            let fresh: Value = serde_json::from_str(tool_text(&fresh)).unwrap();
            assert_eq!(fresh["mandatory"][0]["revision"], 2);
            assert_ne!(fresh["digest"], original["digest"]);
        }
        fixture.finish(true).await;
    }
    Fixture::start_real(&api_url)
        .expect_startup_failure("policy_context_required")
        .await;
    stop.send(()).unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

async fn mock_policy_embed() -> Json<Value> {
    Json(json!({"embeddings":[vec![1.0_f32; 1024]]}))
}

async fn publish_general_fixture_guardrail(pool: &sqlx::PgPool) {
    use chrono::Utc;
    use memory_common::policy::{DeliveryClass, PolicySelectors, PolicyWrite};
    use memoryd::model::{Category, Memory};

    let now = Utc::now();
    let memory = Memory {
        id: uuid::Uuid::from_u128(9000),
        policy: None,
        category: Category::Rule,
        content: "Mandatory fixture guardrail".to_owned(),
        created_at: now,
        embedding: vec![0.0; 1024],
        project: "general".to_owned(),
        summary: "Mandatory fixture guardrail".to_owned(),
        tags: vec![],
        updated_at: now,
    };
    let policy = PolicyWrite {
        policy_key: "fixture.mandatory".to_owned(),
        revision: 1,
        delivery_class: DeliveryClass::Mandatory,
        supersedes: None,
        selectors: PolicySelectors::default(),
        values: std::collections::BTreeMap::new(),
    };
    memoryd::policy::publish_new(pool, &memory, &policy)
        .await
        .unwrap();
}

async fn mock_policy_show() -> Json<Value> {
    Json(json!({"model_info":{"general.architecture":"llama","llama.context_length":8192}}))
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn mcp_read_token_assigns_and_detects_same_minute_edits(pool: sqlx::PgPool) {
    use axum::routing::post;
    use memoryd::api::{ApiState, router};
    use memoryd::app::MemoryApp;
    use memoryd::embed;

    publish_general_fixture_guardrail(&pool).await;

    let embed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let embed_address = embed_listener.local_addr().unwrap();
    let (embed_stop, embed_stopped) = oneshot::channel();
    let embed_server = tokio::spawn(async move {
        axum::serve(
            embed_listener,
            Router::new()
                .route("/api/embed", post(mock_policy_embed))
                .route("/api/show", post(mock_policy_show)),
        )
        .with_graceful_shutdown(async {
            let _ = embed_stopped.await;
        })
        .await
        .unwrap();
    });
    let embed_url = format!("http://{embed_address}");
    let app = MemoryApp::new(
        pool.clone(),
        Arc::new(embed::Client::new(
            embed_url.clone(),
            "test-model".to_owned(),
            None,
            None,
        )),
        "test-model".to_owned(),
        1024,
        reqwest::Client::new(),
        embed_url,
        "test-model".to_owned(),
        1024,
    );
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_address = api_listener.local_addr().unwrap();
    let (api_stop, api_stopped) = oneshot::channel();
    let api_server = tokio::spawn(async move {
        axum::serve(
            api_listener,
            router(ApiState {
                app,
                bearer_token: None,
            }),
        )
        .with_graceful_shutdown(async {
            let _ = api_stopped.await;
        })
        .await
        .unwrap();
    });

    let mut fixture = Fixture::start_real(&format!("http://{api_address}"));
    fixture.initialize(VERSIONS[3]).await;
    let stored = fixture
        .call(
            "memory_store",
            json!({
                "category":"rule","project":"policy-contract","content":"Original text",
                "summary":"Original summary","tags":["initial"]
            }),
        )
        .await;
    fixture.reconnect(VERSIONS[3]).await;
    let id = tool_text(&stored).split_whitespace().nth(2).unwrap();
    let id = uuid::Uuid::parse_str(id).unwrap();
    sqlx::query("UPDATE memories SET updated_at = $2 WHERE id = $1")
        .bind(id)
        .bind(chrono::DateTime::parse_from_rfc3339("2099-09-26T12:34:27.123456Z").unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let fetched = fixture.call("memory_get", json!({"id":id})).await;
    let token = metadata_header(tool_text(&fetched), "updated_at").to_owned();
    assert_eq!(token, "2099-09-26T12:34:27.123456000Z");
    let root_policy =
        json!({"policy_key":"build.storage","revision":1,"delivery_class":"contextual"});
    let precision_mismatch = fixture
        .call(
            "memory_update",
            json!({
                "id":id,"expected_updated_at":"2099-09-26T12:34:27.123456001Z",
                "policy":root_policy
            }),
        )
        .await;
    assert_eq!(
        precision_mismatch["error"]["data"]["code"],
        "policy_stale_assignment"
    );
    for (request, expected_code) in [
        (
            json!({"id":id,"policy":root_policy}),
            "policy_missing_precondition",
        ),
        (
            json!({"id":id,"expected_updated_at":"2099-09-26T12:34:27.123456000","policy":root_policy}),
            "policy_invalid_metadata",
        ),
        (
            json!({"id":id,"expected_updated_at":"2099-09-26T12:34:27.1234560001Z","policy":root_policy}),
            "policy_invalid_metadata",
        ),
        (
            json!({"id":id,"expected_updated_at":token}),
            "policy_invalid_precondition",
        ),
    ] {
        let rejected = fixture.call("memory_update", request).await;
        assert_eq!(rejected["error"]["code"], -32008);
        assert_eq!(rejected["error"]["data"]["code"], expected_code);
    }
    let unchanged = fixture.call("memory_get", json!({"id":id})).await;
    assert!(tool_text(&unchanged).contains("policy: null"));
    assert_eq!(metadata_header(tool_text(&unchanged), "updated_at"), token);
    let assigned = fixture
        .call(
            "memory_update",
            json!({
                "id":id,"expected_updated_at":token,
                "policy":root_policy
            }),
        )
        .await;
    assert!(assigned.get("error").is_none(), "{assigned}");
    fixture.reconnect(VERSIONS[3]).await;
    let verified = fixture.call("memory_get", json!({"id":id})).await;
    assert!(tool_text(&verified).contains("policy_key: build.storage"));
    assert!(tool_text(&verified).contains(&format!("ID: {id}")));

    let revised = fixture
        .call(
            "memory_store",
            json!({
                "category":"rule","project":"policy-contract","content":"Revised text",
                "summary":"Revised summary","tags":["initial"],
                "policy":{"policy_key":"build.storage","revision":2,
                          "delivery_class":"mandatory","supersedes":id}
            }),
        )
        .await;
    assert!(revised.get("error").is_none(), "{revised}");
    fixture.reconnect(VERSIONS[3]).await;
    let successor =
        uuid::Uuid::parse_str(tool_text(&revised).split_whitespace().nth(2).unwrap()).unwrap();
    let history = fixture.call("memory_get", json!({"id":id})).await;
    assert!(tool_text(&history).contains("state: superseded"));
    let current = fixture.call("memory_get", json!({"id":successor})).await;
    assert!(tool_text(&current).contains("delivery_class: mandatory"));
    assert!(tool_text(&current).contains(&format!("supersedes: {id}")));
    for name in ["memory_rules", "memory_bootstrap"] {
        let response = fixture
            .call(
                name,
                json!({"project":"policy-contract","include_general":false}),
            )
            .await;
        let text = tool_text(&response);
        assert!(text.contains(&format!("ID: {successor}")));
        assert!(!text.contains(&format!("ID: {id}")));
    }
    let listed = fixture
        .call("memory_list", json!({"project":"policy-contract"}))
        .await;
    assert!(tool_text(&listed).contains("state: superseded"));
    assert!(tool_text(&listed).contains("state: active"));
    let immutable = fixture
        .call("memory_update", json!({"id":id,"tags":["changed"]}))
        .await;
    assert_eq!(
        immutable["error"]["data"]["code"],
        "policy_immutable_revision"
    );
    let stale_head = fixture
        .call(
            "memory_store",
            json!({
                "category":"rule","project":"policy-contract","content":"Wrong predecessor",
                "summary":"Wrong predecessor","tags":[],
                "policy":{"policy_key":"build.storage","revision":3,
                          "delivery_class":"contextual","supersedes":id}
            }),
        )
        .await;
    assert_eq!(stale_head["error"]["data"]["code"], "policy_stale_head");

    for field in ["content", "summary", "tags"] {
        let stored = fixture
            .call(
                "memory_store",
                json!({
                    "category":"rule","project":"policy-contract","content":"Before",
                    "summary":"Before","tags":["before"]
                }),
            )
            .await;
        fixture.reconnect(VERSIONS[3]).await;
        let id =
            uuid::Uuid::parse_str(tool_text(&stored).split_whitespace().nth(2).unwrap()).unwrap();
        sqlx::query("UPDATE memories SET updated_at = $2 WHERE id = $1")
            .bind(id)
            .bind(chrono::DateTime::parse_from_rfc3339("2099-09-26T12:34:27.123456Z").unwrap())
            .execute(&pool)
            .await
            .unwrap();
        let before = fixture.call("memory_get", json!({"id":id})).await;
        let old_token = metadata_header(tool_text(&before), "updated_at").to_owned();
        let old_minute = metadata_header(tool_text(&before), "Updated").to_owned();
        let edit = match field {
            "content" => json!({"id":id,"content":"After content"}),
            "summary" => json!({"id":id,"summary":"After summary"}),
            _ => json!({"id":id,"tags":["after"]}),
        };
        let changed = fixture.call("memory_update", edit).await;
        assert!(changed.get("error").is_none(), "{changed}");
        fixture.reconnect(VERSIONS[3]).await;
        let after = fixture.call("memory_get", json!({"id":id})).await;
        let new_token = metadata_header(tool_text(&after), "updated_at").to_owned();
        assert_ne!(new_token, old_token);
        assert_eq!(metadata_header(tool_text(&after), "Updated"), old_minute);

        let policy = json!({"policy_key":format!("test.{field}"),"revision":1,"delivery_class":"contextual"});
        let stale = fixture
            .call(
                "memory_update",
                json!({"id":id,"expected_updated_at":old_token,"policy":policy}),
            )
            .await;
        assert_eq!(stale["error"]["code"], -32009);
        assert_eq!(stale["error"]["data"]["code"], "policy_stale_assignment");
        let inspected = fixture.call("memory_get", json!({"id":id})).await;
        assert!(tool_text(&inspected).contains("policy: null"));
        match field {
            "content" => assert!(tool_text(&inspected).contains("After content")),
            "summary" => assert!(tool_text(&inspected).contains("After summary")),
            _ => assert!(tool_text(&inspected).contains("Tags: after")),
        }
        assert_eq!(
            metadata_header(tool_text(&inspected), "updated_at"),
            new_token
        );
        let retry = fixture
            .call(
                "memory_update",
                json!({"id":id,"expected_updated_at":new_token,"policy":policy}),
            )
            .await;
        assert!(retry.get("error").is_none(), "{retry}");
        fixture.reconnect(VERSIONS[3]).await;
    }

    fixture.finish(true).await;
    api_stop.send(()).unwrap();
    embed_stop.send(()).unwrap();
    timeout(DEADLINE, api_server).await.unwrap().unwrap();
    timeout(DEADLINE, embed_server).await.unwrap().unwrap();
}

#[tokio::test]
async fn adoption_header_schema_and_patch_are_stable_across_protocol_versions() {
    for version in VERSIONS {
        let mut fixture = Fixture::start().await;
        fixture.initialize(version).await;
        let listed = fixture.request("tools/list", json!({})).await;
        let update = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "memory_update")
            .unwrap();
        assert_eq!(
            update["inputSchema"]["properties"]["expected_updated_at"]["format"],
            "date-time"
        );
        assert!(update["inputSchema"]["properties"].get("policy").is_some());
        fixture
            .call(
                "memory_store",
                json!({
                    "category":"rule","project":"fixture","content":"Legacy policy",
                    "summary":"Legacy policy","tags":["lang:rust"]
                }),
            )
            .await;
        let invalidated = fixture.call("memory_guardrails", json!({})).await;
        assert_eq!(invalidated["error"]["data"]["code"], "guardrails_changed");
        fixture.reconnect(version).await;
        let fetched = fixture.call("memory_get", json!({"id":ID})).await;
        let token = metadata_header(tool_text(&fetched), "updated_at").to_owned();
        assert_eq!(token, "2025-06-15T12:00:27.123456000Z");
        assert!(tool_text(&fetched).contains("policy: null"));
        let assigned = fixture
            .call("memory_update", json!({
                "id":ID,"expected_updated_at":token,
                "policy":{"policy_key":"build.storage","revision":1,"delivery_class":"contextual"}
            }))
            .await;
        assert!(assigned.get("error").is_none(), "{assigned}");
        let requests = fixture.state.requests.lock().unwrap().clone();
        let patch = requests
            .iter()
            .find(|request| request["method"] == "PATCH")
            .unwrap();
        assert_eq!(patch["body"]["expected_updated_at"], token);
        assert_eq!(patch["body"]["policy"]["policy_key"], "build.storage");
        fixture.finish(true).await;
    }
}

#[tokio::test]
async fn rust_rules_keep_project_placeholder_and_general_policies() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    for shadow_general in [None, Some(true), Some(false)] {
        let mut arguments = json!({"project":"cockpit","tags":["lang:rust"]});
        if let Some(value) = shadow_general {
            arguments["shadow_general"] = json!(value);
        }
        let response = fixture.call("memory_rules", arguments).await;
        let content = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(content.contains("## Rule Set (3 rules)"));
        assert!(content.contains("Rust rules loaded"));
        assert!(content.contains("Keep Rust build artifacts off tmpfs"));
        assert!(content.contains("Run Rust verification checks"));
    }
    let requests = fixture.state.requests.lock().unwrap().clone();
    let rule_requests = requests
        .iter()
        .filter(|request| request["uri"].as_str().unwrap().contains("/cockpit/rules"))
        .collect::<Vec<_>>();
    assert_eq!(rule_requests.len(), 3);
    assert!(
        rule_requests[0]["uri"]
            .as_str()
            .unwrap()
            .contains("shadow_general=true")
    );
    assert!(
        rule_requests[1]["uri"]
            .as_str()
            .unwrap()
            .contains("shadow_general=true")
    );
    assert!(
        rule_requests[2]["uri"]
            .as_str()
            .unwrap()
            .contains("shadow_general=false")
    );
    fixture.finish(true).await;
}

#[tokio::test]
async fn legacy_sessions() {
    for version in VERSIONS {
        let mut fixture = Fixture::start().await;
        fixture.initialize(version).await;
        fixture.request("ping", json!({})).await;
        let mut listed = fixture.request("tools/list", json!({})).await;
        let tools = listed["result"]["tools"].as_array_mut().unwrap();
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        assert_eq!(tools.len(), 18);
        golden("tools", &listed["result"]);
        // Tool ordering is unspecified; compare discovery separately by name.
        fixture.transcript.pop();
        fixture
            .request("tools/call", json!({"name":"memory_server_version"}))
            .await;
        fixture.call("memory_server_version", json!({})).await;
        fixture.call("memory_get", json!({"id":MISSING})).await;
        let error = fixture
            .call("memory_list", json!({"project":"failure"}))
            .await;
        assert_eq!(
            error["error"],
            json!({"code":-32002,"message":"transport error: fixture unavailable"})
        );
        golden(version, &fixture.finish(true).await);
    }
}

#[tokio::test]
async fn guardrails_match_instructions_descriptors_and_tool() {
    for version in VERSIONS {
        let mut fixture = Fixture::start().await;
        fixture.initialize(version).await;
        let pack = fixture_guardrails();
        let digest = pack["digest"].as_str().unwrap();
        let exact = "Use the exact mandatory fixture policy.\nKeep its text intact.";
        let instructions = fixture.transcript[0]["response"]["result"]["instructions"]
            .as_str()
            .unwrap();
        assert!(instructions.contains(digest));
        assert!(instructions.contains(exact));
        let listed = fixture.request("tools/list", json!({})).await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 18);
        for tool in tools {
            let description = tool["description"].as_str().unwrap();
            assert!(description.contains(digest), "{}", tool["name"]);
            assert!(description.contains(exact), "{}", tool["name"]);
            assert!(
                description.contains("fixture.mandatory"),
                "{}",
                tool["name"]
            );
            assert_eq!(tool["_meta"]["memory.server/guardrailsDigest"], digest);
        }
        let result = fixture.call("memory_guardrails", json!({})).await;
        let decoded: Value = serde_json::from_str(tool_text(&result)).unwrap();
        assert_eq!(decoded, pack);
        let omitted = fixture
            .request("tools/call", json!({"name":"memory_guardrails"}))
            .await;
        let omitted_pack: Value = serde_json::from_str(tool_text(&omitted)).unwrap();
        assert_eq!(omitted_pack, pack);
        if version >= VERSIONS[2] {
            assert_eq!(result["result"]["structuredContent"], pack);
        }
        fixture.finish(true).await;
    }
}

#[tokio::test]
async fn guardrail_change_invalidates_until_reconnect() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    *fixture.state.guardrail_override.lock().unwrap() = Some(fixture_guardrails_revision(2));
    for (method, params) in [
        (
            "tools/call",
            json!({"name":"memory_guardrails","arguments":{}}),
        ),
        ("tools/list", json!({})),
        (
            "tools/call",
            json!({"name":"memory_store","arguments":{"project":"fixture","category":"decision","content":"blocked","summary":"blocked"}}),
        ),
    ] {
        let response = fixture.request(method, params).await;
        assert_eq!(response["error"]["data"]["code"], "guardrails_changed");
    }
    assert!(fixture.state.memory.lock().unwrap().is_none());
    *fixture.state.guardrail_override.lock().unwrap() = None;
    let still_invalid = fixture.request("tools/list", json!({})).await;
    assert_eq!(still_invalid["error"]["data"]["code"], "guardrails_changed");
    *fixture.state.guardrail_override.lock().unwrap() = Some(fixture_guardrails_revision(2));
    fixture.reconnect(VERSIONS[3]).await;
    let result = fixture.call("memory_guardrails", json!({})).await;
    let pack: Value = serde_json::from_str(tool_text(&result)).unwrap();
    assert_eq!(pack["mandatory"][0]["revision"], 2);
    fixture.finish(true).await;
}

#[tokio::test]
async fn startup_rejects_old_empty_malformed_and_oversized_peers() {
    for (response, code) in [
        (Value::Null, "guardrails_upstream_unsupported"),
        (json!({"schema_version":1}), "guardrails_malformed"),
        (
            {
                let mut pack = fixture_guardrails();
                pack["mandatory"] = json!([]);
                pack
            },
            "guardrails_empty",
        ),
        (json!("x".repeat(33 * 1024)), "guardrails_too_large"),
        (json!("unauthorized"), "guardrails_transport"),
        (json!("failure"), "guardrails_transport"),
        (json!("timeout"), "guardrails_transport"),
    ] {
        let state = Backend::default();
        *state.guardrail_override.lock().unwrap() = Some(response);
        Fixture::start_with_state(state)
            .await
            .expect_startup_failure(code)
            .await;
    }
}

#[tokio::test]
async fn every_mutation_route_requires_the_live_pack() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    *fixture.state.guardrail_override.lock().unwrap() = Some(fixture_guardrails_revision(2));
    for (name, arguments) in [
        ("memory_delete", json!({"id":ID})),
        (
            "memory_store",
            json!({"project":"fixture","category":"decision","content":"x","summary":"x"}),
        ),
        ("memory_update", json!({"id":ID,"summary":"changed"})),
        (
            "session_log_store",
            json!({"session_id":"s","content":"x","summary":"x"}),
        ),
        ("session_start", json!({"external_session_id":"s"})),
        (
            "session_message_append",
            json!({"session_id":ID,"role":"user","content":"x"}),
        ),
        ("session_finalize", json!({"session_id":ID})),
        (
            "review_submit",
            json!({"memory_id":ID,"reviewer":"r","verdict":"approved","notes":"x"}),
        ),
    ] {
        let response = fixture.call(name, arguments).await;
        assert_eq!(
            response["error"]["data"]["code"], "guardrails_changed",
            "{name}"
        );
    }
    {
        let requests = fixture.state.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request["uri"].as_str().unwrap().contains("/guardrails"))
        );
    }
    fixture.finish(true).await;
}

#[tokio::test]
async fn concurrent_guarded_requests_share_a_permanent_invalidation() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    *fixture.state.guardrail_override.lock().unwrap() = Some(fixture_guardrails_revision(2));
    fixture
        .send(&json!({"jsonrpc":"2.0","id":"list","method":"tools/list","params":{}}))
        .await;
    fixture.send(&json!({"jsonrpc":"2.0","id":"pack","method":"tools/call","params":{"name":"memory_guardrails","arguments":{}}})).await;
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..2 {
        let line = timeout(DEADLINE, fixture.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["error"]["data"]["code"], "guardrails_changed");
        ids.insert(response["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        ids,
        std::collections::BTreeSet::from(["list".to_owned(), "pack".to_owned()])
    );
    fixture.finish(true).await;
}

#[tokio::test]
async fn negotiation_and_eof() {
    for version in ["2026-07-28", "2099-01-01"] {
        let mut fixture = Fixture::start().await;
        fixture.initialize(version).await;
        fixture.request("ping", json!({})).await;
        golden(version, &fixture.finish(true).await);
    }
    golden(
        "eof-before-initialize",
        &Fixture::start().await.finish(false).await,
    );
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    fixture.send(&json!({"jsonrpc":"2.0","id":"pending","method":"tools/call","params":{"name":"memory_list","arguments":{"project":"hold"}}})).await;
    timeout(DEADLINE, fixture.state.entered.notified())
        .await
        .unwrap();
    golden("eof-in-flight", &fixture.finish(true).await);
}

#[tokio::test]
async fn invalid_arguments_do_not_reach_backend() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    for (name, args) in [
        ("memory_get", json!({})),
        ("memory_get", json!({"id":"invalid"})),
        (
            "memory_store",
            json!({"category":"invalid","project":"fixture","content":"x","summary":"x"}),
        ),
        (
            "memory_list",
            json!({"project":"fixture","limit":"invalid"}),
        ),
        (
            "memory_search",
            json!({"project":"fixture","query":"x","graph_hops":-1}),
        ),
        ("unknown", json!({})),
    ] {
        let response = fixture.call(name, args).await;
        assert_eq!(response["error"]["code"], -32602);
    }
    assert_eq!(
        *fixture.state.requests.lock().unwrap(),
        vec![startup_request()]
    );
    golden("invalid-arguments", &fixture.finish(true).await);
}

#[tokio::test]
async fn tool_roundtrips_and_options() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    fixture
        .call("review_queue", json!({"project":"fixture"}))
        .await;
    fixture.call("memory_store", json!({"project":"fixture","category":"decision","content":"Before","summary":"Stored","tags":["review-needed"]})).await;
    fixture
        .call("memory_get", json!({"id":ID.to_uppercase()}))
        .await;
    fixture
        .call(
            "memory_update",
            json!({"id":ID,"content":"After","summary":"Updated","tags":["changed"]}),
        )
        .await;
    fixture.call("memory_get", json!({"id":ID})).await;
    assert_eq!(
        fixture.state.memory.lock().unwrap().as_ref().unwrap()["content"],
        "After"
    );
    fixture.call("memory_list", json!({"project":"fixture","category":"decision","limit":"42","offset":"3","tags":["changed"]})).await;
    fixture
        .call("memory_neighbors", json!({"id":ID,"limit":"2"}))
        .await;
    for limit in [Value::Null, json!("0"), json!("101")] {
        fixture
            .call(
                "review_queue",
                json!({"project":"fixture","limit":limit,"category":"decision"}),
            )
            .await;
    }
    fixture.call("review_submit", json!({"memory_id":ID,"project":"override","reviewer":"fixture-reviewer","verdict":" APPROVED ","notes":"Exact notes\nsecond line"})).await;
    fixture
        .call("memory_rules", json!({"project":"fixture"}))
        .await;
    fixture
        .call("memory_bootstrap", json!({"project":"fixture"}))
        .await;
    fixture
        .call("memory_recall", json!({"project":"fixture"}))
        .await;
    fixture
        .call(
            "memory_search",
            json!({"project":"fixture","query":"empty"}),
        )
        .await;
    for option in [Value::Null, json!(false), json!(true)] {
        fixture.call("memory_rules", json!({"project":"fixture","include_general":option,"shadow_general":option,"tags":["lang:rust"]})).await;
        fixture
            .call(
                "memory_bootstrap",
                json!({"project":"fixture","include_general":option,"include_recall":option}),
            )
            .await;
        fixture
            .call(
                "memory_recall",
                json!({"project":"fixture","include_workflow_artifacts":option}),
            )
            .await;
        fixture.call("memory_search", json!({"project":"fixture","query":"hit","category":"decision","cross_project":option,"expand_query":option,"include_general":option,"include_workflow_artifacts":option,"rerank":option,"graph_hops":"2","limit":"5","min_similarity":0.5,"project_allowlist":["other"],"tags":["tag"]})).await;
    }
    fixture
        .call(
            "memory_search",
            json!({"project":"fixture","query":"fallback"}),
        )
        .await;
    fixture.call("memory_delete", json!({"id":ID})).await;
    assert!(fixture.state.memory.lock().unwrap().is_none());
    fixture.reconnect(VERSIONS[3]).await;
    for (name, args) in [
        ("memory_get", json!({"id":ID})),
        ("memory_update", json!({"id":ID,"content":"missing"})),
        ("memory_delete", json!({"id":ID})),
        (
            "review_submit",
            json!({"memory_id":MISSING,"reviewer":"fixture","verdict":"rejected","notes":"missing"}),
        ),
        ("session_finalize", json!({"session_id":MISSING})),
    ] {
        let response = fixture.call(name, args).await;
        assert_eq!(response["result"]["isError"], true);
        assert!(response.get("error").is_none());
    }
    golden("roundtrips", &fixture.finish(true).await);
}

#[tokio::test]
async fn discovery_and_inline_versions_are_bounded() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": VERSIONS[3],
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let discovery = fixture
        .request("server/discover", json!({"_meta":meta}))
        .await;
    assert_eq!(discovery["result"]["supportedVersions"], json!(VERSIONS));
    assert_eq!(discovery["result"]["capabilities"], json!({"tools":{}}));
    for version in ["2026-07-28", "2099-01-01"] {
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        for (method, mut params) in [
            ("server/discover", json!({})),
            ("tools/list", json!({})),
            (
                "tools/call",
                json!({"name":"memory_server_version","arguments":{}}),
            ),
        ] {
            params["_meta"] = meta.clone();
            let response = fixture.request(method, params).await;
            assert_eq!(
                response["error"],
                json!({
                    "code":-32022,"message":"Unsupported protocol version",
                    "data":{"requested":version,"supported":VERSIONS}
                })
            );
        }
    }
    assert_eq!(
        *fixture.state.requests.lock().unwrap(),
        vec![startup_request()]
    );
    // Rejected inline requests must not promote the negotiated legacy session.
    let response = fixture
        .call("memory_list", json!({"project":"failure"}))
        .await;
    assert_eq!(response["error"]["code"], -32002);
    fixture.finish(true).await;
}

#[tokio::test]
async fn discovery_opener_advertises_only_legacy_versions() {
    let mut fixture = Fixture::start().await;
    let response = fixture
        .request(
            "server/discover",
            json!({"_meta":{
                "io.modelcontextprotocol/protocolVersion":VERSIONS[3],
                "io.modelcontextprotocol/clientCapabilities":{}
            }}),
        )
        .await;
    assert_eq!(response["result"]["supportedVersions"], json!(VERSIONS));
    assert_eq!(response["result"]["capabilities"], json!({"tools":{}}));
    assert_eq!(
        *fixture.state.requests.lock().unwrap(),
        vec![startup_request()]
    );
    fixture.finish(true).await;
}

#[tokio::test]
async fn uuid_forms_forward_the_same_canonical_id() {
    let mut fixture = Fixture::start().await;
    fixture.initialize(VERSIONS[3]).await;
    let canonical = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    for input in [
        "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
        "aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaaa",
        "urn:uuid:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    ] {
        let response = fixture.call("memory_get", json!({"id":input})).await;
        assert_eq!(
            response["result"],
            json!({
                "content":[{"type":"text","text":format!("Memory {canonical} not found")}],
                "isError":true
            })
        );
    }
    let expected = json!({
        "method":"GET","uri":format!("/api/v1/memories/{canonical}"),"body":null
    });
    let mut requests = vec![startup_request()];
    requests.extend(vec![expected; 3]);
    assert_eq!(*fixture.state.requests.lock().unwrap(), requests);
    fixture.finish(true).await;
}
