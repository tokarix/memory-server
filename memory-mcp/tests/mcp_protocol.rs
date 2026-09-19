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

#[derive(Clone, Default)]
struct Backend {
    memory: Arc<Mutex<Option<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

fn memory() -> Value {
    json!({"id":ID,"project":"fixture","category":"decision","summary":"Choice",
        "content":"Use a synthetic backend.","tags":["review-needed"],
        "created_at":"2025-06-15T12:00:00Z","updated_at":"2025-06-15T12:00:00Z"})
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
        let state = Backend::default();
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
        let config =
            std::env::temp_dir().join(format!("mcp-contract-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&config, format!("memoryd_url = \"http://{address}\"\n")).unwrap();
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
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(*actual, expected, "rmcp 1.5 wire contract: {name}");
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
        assert_eq!(tools.len(), 17);
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
    assert!(fixture.state.requests.lock().unwrap().is_empty());
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
    assert!(fixture.state.requests.lock().unwrap().is_empty());
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
    assert!(fixture.state.requests.lock().unwrap().is_empty());
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
    assert_eq!(*fixture.state.requests.lock().unwrap(), vec![expected; 3]);
    fixture.finish(true).await;
}
