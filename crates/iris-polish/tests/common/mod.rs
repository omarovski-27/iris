//! Offline test doubles shared by the integration suites.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_polish::{ChatTransport, PolishError, TransportFuture};

/// What a [`StubTransport`] does when called.
#[derive(Clone, Debug)]
pub enum Stub {
    /// Return this body verbatim.
    Body(String),
    /// Return a well-formed chat completion whose content is this string.
    Content(String),
    /// Fail with this error.
    Fail(String),
    /// Never answer: the caller's timeout has to save it.
    Hang,
}

/// A [`ChatTransport`] that records what it was asked and answers from a script.
#[derive(Debug)]
pub struct StubTransport {
    stub: Stub,
    delay: Duration,
    calls: Mutex<Vec<Call>>,
}

/// One recorded request.
#[derive(Clone, Debug)]
pub struct Call {
    pub url: String,
    pub api_key: String,
    pub body: String,
}

impl StubTransport {
    pub fn new(stub: Stub) -> Arc<Self> {
        Arc::new(Self {
            stub,
            delay: Duration::ZERO,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Answer with a well-formed completion carrying `content`.
    pub fn returning(content: impl Into<String>) -> Arc<Self> {
        Self::new(Stub::Content(content.into()))
    }

    /// Answer with this exact response body.
    pub fn with_body(body: impl Into<String>) -> Arc<Self> {
        Self::new(Stub::Body(body.into()))
    }

    /// Fail every call.
    pub fn failing(message: impl Into<String>) -> Arc<Self> {
        Self::new(Stub::Fail(message.into()))
    }

    /// Never answer.
    pub fn hanging() -> Arc<Self> {
        Self::new(Stub::Hang)
    }

    /// Wait `delay` before answering.
    pub fn after(stub: Stub, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            stub,
            delay,
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn last_call(&self) -> Call {
        self.calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("transport was never called")
    }
}

impl ChatTransport for StubTransport {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        api_key: &'a str,
        body: &'a str,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(Call {
                url: url.to_string(),
                api_key: api_key.to_string(),
                body: body.to_string(),
            });

            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }

            match &self.stub {
                Stub::Body(body) => Ok(body.clone()),
                Stub::Content(content) => Ok(completion(content)),
                Stub::Fail(message) => Err(PolishError::Transport(message.clone())),
                Stub::Hang => {
                    // Long enough that any real budget fires first.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    unreachable!("hanging transport was allowed to finish")
                }
            }
        })
    }
}

/// A response body shaped exactly like Groq's and OpenAI's.
pub fn completion(content: &str) -> String {
    let escaped = content
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!(
        r#"{{"id":"chatcmpl-test","object":"chat.completion","created":1700000000,
"model":"llama-3.1-8b-instant","choices":[{{"index":0,"message":{{"role":"assistant",
"content":"{escaped}"}},"logprobs":null,"finish_reason":"stop"}}],
"usage":{{"prompt_tokens":120,"completion_tokens":8,"total_tokens":128}}}}"#
    )
}
