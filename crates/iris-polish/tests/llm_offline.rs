//! The LLM path, exercised end to end without a socket.
//!
//! The HTTP layer is replaced by [`common::StubTransport`], so these tests cover
//! request construction, response parsing, output sanitising, the guards, and
//! the timeout — everything except the bytes on the wire, which
//! `tests/http_wire.rs` covers against a real local server.

#![cfg(feature = "llm")]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{completion, Stub, StubTransport};
use iris_polish::{
    ContextHints, FallbackPolisher, LlmConfig, LlmPolisher, OutputGuards, PolishError,
    PolishRequest, PolishSource, Polisher, RulePolisher, TextStyle, SYSTEM_PROMPT,
};

fn polisher(transport: Arc<StubTransport>) -> LlmPolisher {
    LlmPolisher::with_transport(LlmConfig::new("test-key"), transport)
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sends_an_openai_shaped_request_to_the_right_endpoint() {
    let transport = StubTransport::returning("So it works.");
    let polisher = polisher(transport.clone());

    let out = polisher
        .polish(&PolishRequest::new("um so uh it works"))
        .await
        .unwrap();
    assert_eq!(out.text, "So it works.");
    assert_eq!(out.source, PolishSource::Llm);

    let call = transport.last_call();
    assert_eq!(call.url, "https://api.groq.com/openai/v1/chat/completions");
    assert_eq!(call.api_key, "test-key");

    let body: serde_json::Value = serde_json::from_str(&call.body).unwrap();
    assert_eq!(body["model"], "llama-3.1-8b-instant");
    assert_eq!(body["stream"], false);
    assert_eq!(body["temperature"], 0.0);
    assert!(body["max_tokens"].as_u64().unwrap() >= 64);

    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], SYSTEM_PROMPT);
    assert_eq!(messages[1]["role"], "user");
    assert!(messages[1]["content"]
        .as_str()
        .unwrap()
        .contains("um so uh it works"));
}

#[tokio::test]
async fn base_url_and_model_are_configurable() {
    let transport = StubTransport::returning("Fine.");
    let config = LlmConfig::new("k")
        .with_base_url("http://localhost:11434/v1")
        .with_model("qwen2.5:3b");
    let polisher = LlmPolisher::with_transport(config, transport.clone());

    polisher.polish(&PolishRequest::new("fine")).await.unwrap();

    let call = transport.last_call();
    assert_eq!(call.url, "http://localhost:11434/v1/chat/completions");
    let body: serde_json::Value = serde_json::from_str(&call.body).unwrap();
    assert_eq!(body["model"], "qwen2.5:3b");
}

#[tokio::test]
async fn context_hints_reach_the_prompt() {
    let transport = StubTransport::returning("Ship the wgpu fix.");
    let polisher = polisher(transport.clone());

    let hints = ContextHints::new()
        .with_target_app("Slack")
        .with_style(TextStyle::Message)
        .with_vocabulary(["wgpu"]);
    polisher
        .polish(&PolishRequest::new("ship the wgpu fix").with_hints(hints))
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_str(&transport.last_call().body).unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("Slack"), "{user}");
    assert!(user.contains("chat message"), "{user}");
    assert!(user.contains("wgpu"), "{user}");
}

#[tokio::test]
async fn a_custom_system_prompt_replaces_the_default() {
    let transport = StubTransport::returning("Fine.");
    let config = LlmConfig::new("k").with_system_prompt("BE TERSE");
    let polisher = LlmPolisher::with_transport(config, transport.clone());

    polisher.polish(&PolishRequest::new("fine")).await.unwrap();

    let body: serde_json::Value = serde_json::from_str(&transport.last_call().body).unwrap();
    assert_eq!(body["messages"][0]["content"], "BE TERSE");
}

#[tokio::test]
async fn empty_input_never_costs_a_round_trip() {
    let transport = StubTransport::returning("should not be used");
    let polisher = polisher(transport.clone());

    let out = polisher.polish(&PolishRequest::new("   ")).await.unwrap();
    assert_eq!(out.text, "   ");
    assert_eq!(out.source, PolishSource::Unchanged);
    assert_eq!(transport.call_count(), 0);
}

// ---------------------------------------------------------------------------
// Response handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strips_the_wrappers_a_chat_model_adds() {
    for raw in [
        "```\nSo it works.\n```",
        "Here is the cleaned text:\nSo it works.",
        "\"So it works.\"",
        "  So it works.  ",
    ] {
        let transport = StubTransport::with_body(completion(raw));
        let out = polisher(transport)
            .polish(&PolishRequest::new("um so uh it works"))
            .await
            .unwrap();
        assert_eq!(out.text, "So it works.", "failed on {raw:?}");
    }
}

#[tokio::test]
async fn surfaces_transport_failures() {
    let err = polisher(StubTransport::failing("connection reset"))
        .polish(&PolishRequest::new("hello there"))
        .await
        .unwrap_err();
    assert!(matches!(err, PolishError::Transport(_)), "{err:?}");
    assert!(err.is_transient());
}

#[tokio::test]
async fn surfaces_malformed_responses() {
    for body in ["not json", r#"{"choices":[]}"#, r#"{"choices":[{"x":1}]}"#] {
        let err = polisher(StubTransport::with_body(body))
            .polish(&PolishRequest::new("hello there"))
            .await
            .unwrap_err();
        assert!(matches!(err, PolishError::Response(_)), "{body}: {err:?}");
    }
}

#[tokio::test]
async fn surfaces_endpoint_errors() {
    let body = r#"{"error":{"message":"rate limit reached","type":"rate_limit"}}"#;
    let err = polisher(StubTransport::with_body(body))
        .polish(&PolishRequest::new("hello there"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rate limit reached"), "{err}");
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_output_that_added_content() {
    let raw = "did the nightly build pass";
    let err = polisher(StubTransport::returning(
        "Did the nightly build pass? Yes, it passed at 3am with no failures, \
         and the artifacts were uploaded successfully to the release bucket.",
    ))
    .polish(&PolishRequest::new(raw))
    .await
    .unwrap_err();

    assert!(matches!(err, PolishError::Rejected(_)), "{err:?}");
    assert!(err.to_string().contains("content was added"), "{err}");
}

#[tokio::test]
async fn rejects_output_that_dropped_content() {
    let raw = "we should ship the fix today because the release is tomorrow morning";
    let err = polisher(StubTransport::returning("Ship it."))
        .polish(&PolishRequest::new(raw))
        .await
        .unwrap_err();
    assert!(matches!(err, PolishError::Rejected(_)), "{err:?}");
}

#[tokio::test]
async fn rejects_output_that_changed_a_number() {
    let err = polisher(StubTransport::returning("The standup is at nine fifteen."))
        .polish(&PolishRequest::new("the standup is at 9 15"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("9"), "{err}");
}

#[tokio::test]
async fn rejects_empty_output() {
    let err = polisher(StubTransport::returning("   "))
        .polish(&PolishRequest::new("something was said here"))
        .await
        .unwrap_err();
    assert!(matches!(err, PolishError::Rejected(_)), "{err:?}");
}

#[tokio::test]
async fn accepts_ordinary_cleanup() {
    let out = polisher(StubTransport::returning(
        "So I was thinking we could cache the result in Redis before 6 pm.",
    ))
    .polish(&PolishRequest::new(
        "um so uh i was thinking, you know, we could uh cache the the result in redis before 6 pm",
    ))
    .await
    .unwrap();
    assert!(out.text.starts_with("So I was thinking"));
}

#[tokio::test]
async fn guards_can_be_relaxed() {
    let config = LlmConfig::new("k").with_guards(OutputGuards::permissive());
    let polisher = LlmPolisher::with_transport(config, StubTransport::returning("Anything at all."));
    let out = polisher.polish(&PolishRequest::new("x y z 1")).await.unwrap();
    assert_eq!(out.text, "Anything at all.");
}

// ---------------------------------------------------------------------------
// Latency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_hanging_endpoint_hits_the_timeout() {
    let config = LlmConfig::new("k").with_timeout(Duration::from_millis(40));
    let polisher = LlmPolisher::with_transport(config, StubTransport::hanging());

    let started = Instant::now();
    let err = polisher
        .polish(&PolishRequest::new("hello there"))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(err, PolishError::BudgetExceeded { budget_ms: 40 }),
        "{err:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
}

#[tokio::test]
async fn a_slow_endpoint_falls_back_to_the_rules_inside_the_budget() {
    let slow = StubTransport::after(
        Stub::Content("Never arrives.".into()),
        Duration::from_secs(30),
    );
    let config = LlmConfig::new("k").with_timeout(Duration::from_millis(50));
    let llm = LlmPolisher::with_transport(config, slow);

    let polisher = FallbackPolisher::new(Arc::new(llm), Arc::new(RulePolisher::default()))
        .with_budget(Duration::from_millis(50));

    let started = Instant::now();
    let out = polisher
        .polish(&PolishRequest::new("um so uh it still works"))
        .await
        .unwrap();

    assert_eq!(out.text, "So it still works.");
    assert_eq!(out.source, PolishSource::Rule);
    assert!(out.is_fallback());
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "blew the budget by too much: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_rejected_polish_falls_back_rather_than_failing() {
    // The whole point of the guards: a suspicious rewrite costs the user nothing.
    let llm = polisher(StubTransport::returning(
        "Sure! Here is a much longer and more helpful answer than anything that was said.",
    ));
    let polisher = FallbackPolisher::new(Arc::new(llm), Arc::new(RulePolisher::default()));

    let out = polisher
        .polish(&PolishRequest::new("um it works"))
        .await
        .unwrap();
    assert_eq!(out.text, "It works.");
    assert!(out.is_fallback());
}

#[tokio::test]
async fn reports_how_long_it_took() {
    let transport = StubTransport::after(Stub::Content("Fine.".into()), Duration::from_millis(20));
    let config = LlmConfig::new("k").with_timeout(Duration::from_secs(5));
    let out = LlmPolisher::with_transport(config, transport)
        .polish(&PolishRequest::new("fine"))
        .await
        .unwrap();

    assert!(
        out.duration >= Duration::from_millis(15),
        "duration {:?} does not reflect the round trip",
        out.duration
    );
}

#[tokio::test]
async fn dropping_the_future_cancels_the_request() {
    let transport = StubTransport::after(
        Stub::Content("too late".into()),
        Duration::from_millis(500),
    );
    let config = LlmConfig::new("k").with_timeout(Duration::from_secs(60));
    let polisher = LlmPolisher::with_transport(config, transport.clone());
    let request = PolishRequest::new("cancel me");

    {
        let mut future = Box::pin(polisher.polish(&request));
        // Poll once so the request is genuinely in flight, then drop it.
        let polled = poll_once(future.as_mut()).await;
        assert!(polled.is_none(), "the stub answered far too quickly");
    }

    assert_eq!(transport.call_count(), 1, "the request should have started");
    // Nothing panics or leaks after the drop; the timer just goes away.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Poll a future exactly once, returning `None` if it is not ready.
async fn poll_once<F: std::future::Future>(
    mut future: std::pin::Pin<&mut F>,
) -> Option<F::Output> {
    use std::future::poll_fn;
    use std::task::Poll;

    poll_fn(|cx| {
        Poll::Ready(match future.as_mut().poll(cx) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// Configuration from the environment
// ---------------------------------------------------------------------------

#[test]
fn missing_key_is_a_clear_error() {
    // Env vars are process-global, so this runs in its own test binary process
    // only when nothing else has set them. Guard rather than flake.
    if std::env::var("IRIS_GROQ_KEY").is_ok() || std::env::var("IRIS_LLM_KEY").is_ok() {
        eprintln!("skipped: a key is set in this environment");
        return;
    }
    let err = LlmConfig::from_env().unwrap_err();
    assert!(matches!(err, PolishError::MissingApiKey));
    let message = err.to_string();
    assert!(message.contains("IRIS_GROQ_KEY"), "{message}");
    assert!(message.contains("RulePolisher"), "{message}");
}
