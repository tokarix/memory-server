//! Isolated fixtures for the synchronous Hugging Face tokenizer client.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use tokenizers::Tokenizer;
use tokenizers::models::wordlevel::WordLevel;

#[test]
fn tokenizer_fixture_child() {
    let Ok(expected) = std::env::var("MEMORYD_TOKENIZER_FIXTURE") else {
        return;
    };
    let revision =
        std::env::var("MEMORYD_TOKENIZER_REVISION").unwrap_or_else(|_| "main".to_owned());
    // Api::new() does not read HF_HOME/HF_ENDPOINT in hf-hub 0.5.0.
    // Exercise the retained sync transport with explicit fixture isolation.
    let api = hf_hub::api::sync::ApiBuilder::from_env().build().unwrap();
    let repo = api.repo(hf_hub::Repo::with_revision(
        "fixture/tokenizer".to_owned(),
        hf_hub::RepoType::Model,
        revision,
    ));
    let tokenizer = repo
        .get("tokenizer.json")
        .ok()
        .and_then(|path| Tokenizer::from_file(path).ok());
    assert_eq!(tokenizer.is_some(), expected == "loaded");
}

#[test]
fn tokenizer_download_cache_and_failures() {
    // Child processes isolate HF_HOME/HF_ENDPOINT without mutating the test
    // runner's environment or using the user's Hugging Face cache/token.
    let home = std::env::temp_dir().join(format!("memoryd-hf-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let tokenizer = Tokenizer::new(WordLevel::default())
        .to_string(false)
        .unwrap();
    let (stop, stopped) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut paths = Vec::new();
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !matches!(
                        stopped.recv_timeout(Duration::from_millis(10)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            let (status, body) = if path.contains("/missing/") {
                ("404 Not Found", "missing")
            } else if path.contains("/invalid/") {
                ("200 OK", "invalid tokenizer JSON")
            } else {
                ("200 OK", tokenizer.as_str())
            };
            let size = body.len();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {size}\r\nContent-Range: bytes 0-{}/{size}\r\nETag: \"fixture-{size}\"\r\nx-repo-commit: fixture-{size}\r\nConnection: close\r\n\r\n{body}",
                size - 1
            )
            .unwrap();
            paths.push(path);
        }
        paths
    });
    let run = |revision: Option<&str>, expected: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tokenizer_fixture_child"])
            .env("HF_HOME", &home)
            .env("HF_ENDPOINT", &endpoint)
            .env_remove("HF_TOKEN")
            .env("NO_PROXY", "127.0.0.1")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .env("MEMORYD_TOKENIZER_FIXTURE", expected)
            .env_remove("MEMORYD_TOKENIZER_REVISION");
        if let Some(revision) = revision {
            command.env("MEMORYD_TOKENIZER_REVISION", revision);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
    };

    run(Some("pinned"), "loaded");
    run(None, "loaded");
    run(Some("invalid"), "fallback");
    run(Some("missing"), "fallback");
    stop.send(()).unwrap();
    let paths = server.join().unwrap();
    for revision in ["pinned", "main", "invalid", "missing"] {
        assert!(paths.contains(&format!(
            "/fixture/tokenizer/resolve/{revision}/tokenizer.json"
        )));
    }
    // With the server stopped, the downloaded revision must load from HF_HOME.
    run(Some("pinned"), "loaded");
    run(Some("uncached"), "fallback");
    std::fs::remove_dir_all(home).unwrap();
}
