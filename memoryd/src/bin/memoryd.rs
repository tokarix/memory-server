//! Run the memory server HTTP daemon.

use std::sync::Arc;

use axum::Router;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};

use memoryd::{api, app::MemoryApp, config, db, embed, ui};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = match std::env::args().nth(1) {
        Some(path) => config::Config::load(std::path::Path::new(&path)).map_err(|error| {
            tracing::error!("failed to load config: {error:#?}");
            error
        })?,
        None => config::Config::default(),
    };

    tracing::info!(
        "connecting to database: {}",
        config.database_url.split('@').next_back().unwrap_or("?"),
    );
    let pool = db::connect(&config.database_url).await?;

    tracing::info!("running migrations");
    db::migrate(&pool).await?;

    let app = MemoryApp::new(
        pool,
        Arc::new(embed::Client::new(
            config.ollama_url.clone(),
            config.embedding_model,
            config.embedding_tokenizer_repo,
            config.embedding_tokenizer_revision,
        )),
        config.expand_model,
        config.expand_num_ctx,
        reqwest::Client::new(),
        config.ollama_url,
        config.rerank_model,
        config.rerank_num_ctx,
    );
    let state = api::ApiState {
        app,
        bearer_token: config.api_token,
    };

    let listener = tokio::net::TcpListener::bind(&config.http_bind).await?;
    let router = with_request_tracing(api::router(state.clone()).merge(ui::router(state)));
    tracing::info!("starting memoryd HTTP server on {}", config.http_bind);
    axum::serve(listener, router).await?;

    Ok(())
}

fn with_request_tracing(router: Router) -> Router {
    router.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
            .on_request(DefaultOnRequest::new().level(tracing::Level::INFO))
            .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
    )
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use axum::{Router, http::StatusCode, routing::post};

    use super::with_request_tracing;
    use memoryd::{api, app::MemoryApp, config, embed, ui};

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_state() -> api::ApiState {
        let config = config::Config::default();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        api::ApiState {
            app: MemoryApp::new(
                pool,
                Arc::new(embed::Client::new(
                    config.ollama_url.clone(),
                    config.embedding_model,
                    None,
                    None,
                )),
                config.expand_model,
                config.expand_num_ctx,
                reqwest::Client::new(),
                config.ollama_url,
                config.rerank_model,
                config.rerank_num_ctx,
            ),
            bearer_token: Some("configured-token-secret".to_owned()),
        }
    }

    // A current-thread runtime keeps all server events inside this subscriber's
    // scope without installing a process-global subscriber for other tests.
    #[test]
    fn request_tracing_preserves_responses_classification_and_redaction() {
        let logs = LogBuffer::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let state = test_state();
                // Exercise the real API/UI routers plus a deterministic 5xx
                // fixture, without a database or external model service.
                let router =
                    api::router(state.clone())
                        .merge(ui::router(state))
                        .merge(Router::new().route(
                            "/failure",
                            post(|| async {
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    [("x-private", "response-header-secret")],
                                    "response-body-secret",
                                )
                            }),
                        ));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let (shutdown, stopped) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    axum::serve(listener, with_request_tracing(router))
                        .with_graceful_shutdown(async { stopped.await.unwrap() })
                        .await
                        .unwrap();
                });
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap();
                for (path, status) in [
                    ("/api/v1/health", StatusCode::OK),
                    ("/", StatusCode::SEE_OTHER),
                    ("/api/v1/projects/test/memories", StatusCode::UNAUTHORIZED),
                    ("/missing", StatusCode::NOT_FOUND),
                ] {
                    let response = client
                        .get(format!("http://{address}{path}"))
                        .bearer_auth("request-token-secret")
                        .header("cookie", "session=cookie-secret")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), status);
                    if path == "/" {
                        assert_eq!(response.headers()["location"], "/ui");
                    }
                    let body = response.text().await.unwrap();
                    if path == "/api/v1/health" {
                        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["status"], "ok");
                    }
                }
                let response = client
                    .post(format!("http://{address}/failure"))
                    .body("request-body-secret")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(response.headers()["x-private"], "response-header-secret");
                assert_eq!(response.text().await.unwrap(), "response-body-secret");
                shutdown.send(()).unwrap();
                server.await.unwrap();
            });

        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert_eq!(output.matches("started processing request").count(), 5);
        assert_eq!(output.matches("finished processing request").count(), 5);
        assert_eq!(output.matches("response failed").count(), 1);
        for status in [200, 303, 401, 404, 500] {
            assert!(output.contains(&format!("status={status}")), "{output}");
        }
        for line in output
            .lines()
            .filter(|line| line.contains("tower_http::trace"))
        {
            assert!(line.contains("request{method="), "{line}");
            assert!(line.contains("uri="), "{line}");
            assert!(line.contains("version=HTTP/1.1"), "{line}");
            if line.contains("response failed") {
                assert!(line.contains("ERROR"), "{line}");
                assert!(line.contains("uri=/failure"), "{line}");
                assert!(line.contains("classification=Status code: 500"), "{line}");
            } else {
                assert!(line.contains("INFO"), "{line}");
            }
        }
        assert!(!output.contains("secret"), "{output}");
    }
}
