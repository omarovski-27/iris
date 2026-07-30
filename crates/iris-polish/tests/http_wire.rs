//! The real HTTP client, against a real socket — on loopback, with no network.
//!
//! `tests/llm_offline.rs` stubs the transport out, which is the right level for
//! testing prompt and guard logic but proves nothing about `reqwest` being wired
//! up correctly. These tests run a one-shot HTTP/1.1 server on `127.0.0.1:0` and
//! check the bytes that actually leave the client: the method, the path, the
//! `Authorization` header, and the JSON body.

#![cfg(feature = "llm")]

mod common;

use std::time::Duration;

use common::completion;
use iris_polish::{LlmConfig, LlmPolisher, PolishError, PolishRequest, Polisher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// What the server received.
struct Received {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Serve exactly one request, then hand back what was received.
///
/// Returns the base URL to point a polisher at.
async fn one_shot_server(
    status_line: &'static str,
    body: String,
) -> (String, oneshot::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();

        // Read headers, then exactly Content-Length bytes of body.
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break buffer.len();
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(index) = find(&buffer, b"\r\n\r\n") {
                break index + 4;
            }
        };

        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();

        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);

        let mut body_bytes = buffer[header_end..].to_vec();
        while body_bytes.len() < content_length {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            body_bytes.extend_from_slice(&chunk[..n]);
        }

        let response = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();

        let _ = tx.send(Received {
            request_line,
            headers,
            body: String::from_utf8_lossy(&body_bytes).to_string(),
        });
    });

    (format!("http://{addr}/v1"), rx)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[tokio::test]
async fn posts_a_bearer_authenticated_json_request() {
    let (base_url, received) = one_shot_server("HTTP/1.1 200 OK", completion("So it works.")).await;

    let config = LlmConfig::new("secret-key")
        .with_base_url(base_url)
        .with_timeout(Duration::from_secs(10));
    let polisher = LlmPolisher::new(config).unwrap();

    let out = polisher
        .polish(&PolishRequest::new("um so uh it works"))
        .await
        .unwrap();
    assert_eq!(out.text, "So it works.");

    let received = received.await.unwrap();
    assert!(
        received
            .request_line
            .starts_with("POST /v1/chat/completions "),
        "{}",
        received.request_line
    );
    assert_eq!(received.header("authorization"), Some("Bearer secret-key"));
    assert_eq!(received.header("content-type"), Some("application/json"));
    assert!(received
        .header("user-agent")
        .is_some_and(|ua| ua.starts_with("iris-polish/")));

    let body: serde_json::Value = serde_json::from_str(&received.body).unwrap();
    assert!(body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("um so uh it works"));
}

#[tokio::test]
async fn http_errors_carry_the_status_and_body() {
    let (base_url, _received) = one_shot_server(
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":{"message":"Invalid API Key"}}"#.to_string(),
    )
    .await;

    let config = LlmConfig::new("wrong-key")
        .with_base_url(base_url)
        .with_timeout(Duration::from_secs(10));
    let err = LlmPolisher::new(config)
        .unwrap()
        .polish(&PolishRequest::new("hello there"))
        .await
        .unwrap_err();

    match err {
        PolishError::HttpStatus { status, ref body } => {
            assert_eq!(status, 401);
            assert!(body.contains("Invalid API Key"), "{body}");
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
    assert!(!err.is_transient(), "a bad key is not worth retrying");
}

#[tokio::test]
async fn a_server_that_never_answers_hits_the_budget() {
    // Accept the connection but never write a response.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(socket);
    });

    let config = LlmConfig::new("k")
        .with_base_url(format!("http://{addr}/v1"))
        .with_timeout(Duration::from_millis(80));
    let polisher = LlmPolisher::new(config).unwrap();

    let started = std::time::Instant::now();
    let err = polisher
        .polish(&PolishRequest::new("hello there friend"))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    // Either the crate's own timer or reqwest's fires first; both are correct,
    // and both must be well inside a second.
    assert!(
        matches!(
            err,
            PolishError::BudgetExceeded { .. } | PolishError::Transport(_)
        ),
        "{err:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
}

#[tokio::test]
async fn a_refused_connection_is_a_transport_error() {
    // Bind and immediately drop, so the port is almost certainly closed.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let config = LlmConfig::new("k")
        .with_base_url(format!("http://{addr}/v1"))
        .with_timeout(Duration::from_secs(2));
    let err = LlmPolisher::new(config)
        .unwrap()
        .polish(&PolishRequest::new("hello there"))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            PolishError::Transport(_) | PolishError::BudgetExceeded { .. }
        ),
        "{err:?}"
    );
    assert!(err.is_transient());
}
