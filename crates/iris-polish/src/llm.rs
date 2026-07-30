//! The LLM quality path.
//!
//! One HTTP round trip to an OpenAI-compatible `/chat/completions` endpoint
//! (Groq by default, because it is the fastest widely available one), carrying
//! the prompt in [`crate::prompt`].
//!
//! Two things make this safe to put on the keystroke-to-text path:
//!
//! - **A hard timeout.** [`LlmConfig::timeout`] bounds the request. Past it the
//!   future resolves to [`PolishError::BudgetExceeded`] and the caller — normally
//!   [`FallbackPolisher`](crate::FallbackPolisher) — uses the rule engine's
//!   already-computed result.
//! - **Output guards.** [`OutputGuards`] throws away model output that grew,
//!   shrank, or lost a number, because those are what a rewrite looks like from
//!   the outside. A rejected polish costs the user nothing; an accepted rewrite
//!   costs them their words.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::PolishError;
use crate::polisher::{PolishFuture, Polisher};
use crate::prompt::{build_user_message, SYSTEM_PROMPT};
use crate::types::{PolishRequest, PolishSource, Polished};

/// Groq's OpenAI-compatible base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// The default model: the fastest one that still punctuates well.
pub const DEFAULT_MODEL: &str = "llama-3.1-8b-instant";

/// Preferred API-key variable.
pub const ENV_GROQ_KEY: &str = "IRIS_GROQ_KEY";
/// Provider-neutral API-key variable, used when [`ENV_GROQ_KEY`] is unset.
pub const ENV_KEY: &str = "IRIS_LLM_KEY";
/// Overrides [`DEFAULT_BASE_URL`].
pub const ENV_BASE_URL: &str = "IRIS_LLM_BASE_URL";
/// Overrides [`DEFAULT_MODEL`].
pub const ENV_MODEL: &str = "IRIS_LLM_MODEL";
/// Overrides the request timeout, in milliseconds.
pub const ENV_TIMEOUT_MS: &str = "IRIS_LLM_TIMEOUT_MS";

/// How the LLM path is configured.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// Base URL; `/chat/completions` is appended.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// Bearer token.
    pub api_key: String,
    /// Hard ceiling on the round trip.
    pub timeout: Duration,
    /// Sampling temperature. Defaults to `0.0`: this is a transformation, not
    /// a creative task, and variance here shows up as inconsistent output.
    pub temperature: f32,
    /// Replaces [`SYSTEM_PROMPT`] when set.
    pub system_prompt: Option<String>,
    /// Guards applied to the model's output before it is accepted.
    pub guards: OutputGuards,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: String::new(),
            timeout: crate::fallback::DEFAULT_LATENCY_BUDGET,
            temperature: 0.0,
            system_prompt: None,
            guards: OutputGuards::default(),
        }
    }
}

impl LlmConfig {
    /// Configure against Groq's defaults with an explicit key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// Read the configuration from the environment.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `IRIS_GROQ_KEY` | API key (preferred) |
    /// | `IRIS_LLM_KEY` | API key (any provider) |
    /// | `IRIS_LLM_BASE_URL` | base URL, default [`DEFAULT_BASE_URL`] |
    /// | `IRIS_LLM_MODEL` | model, default [`DEFAULT_MODEL`] |
    /// | `IRIS_LLM_TIMEOUT_MS` | timeout, default 150 ms |
    ///
    /// Returns [`PolishError::MissingApiKey`] when there is no key, which is the
    /// signal for callers to fall back to [`RulePolisher`](crate::RulePolisher).
    pub fn from_env() -> Result<Self, PolishError> {
        let api_key = read_env(ENV_GROQ_KEY)
            .or_else(|| read_env(ENV_KEY))
            .ok_or(PolishError::MissingApiKey)?;

        let mut config = Self::new(api_key);
        if let Some(url) = read_env(ENV_BASE_URL) {
            config.base_url = url;
        }
        if let Some(model) = read_env(ENV_MODEL) {
            config.model = model;
        }
        if let Some(raw) = read_env(ENV_TIMEOUT_MS) {
            let ms: u64 = raw.parse().map_err(|_| {
                PolishError::Config(format!("{ENV_TIMEOUT_MS} must be a whole number of milliseconds, got {raw:?}"))
            })?;
            if ms == 0 {
                return Err(PolishError::Config(format!("{ENV_TIMEOUT_MS} must be greater than zero")));
            }
            config.timeout = Duration::from_millis(ms);
        }
        config.validate()?;
        Ok(config)
    }

    /// Override the base URL.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Override the request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Replace the system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Replace the output guards.
    #[must_use]
    pub fn with_guards(mut self, guards: OutputGuards) -> Self {
        self.guards = guards;
        self
    }

    /// Reject configurations that cannot produce a request.
    pub fn validate(&self) -> Result<(), PolishError> {
        if self.api_key.trim().is_empty() {
            return Err(PolishError::MissingApiKey);
        }
        if self.model.trim().is_empty() {
            return Err(PolishError::Config("model must not be empty".into()));
        }
        let url = self.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(PolishError::Config(format!(
                "base URL must start with http:// or https://, got {:?}",
                self.base_url
            )));
        }
        if self.timeout.is_zero() {
            return Err(PolishError::Config("timeout must be greater than zero".into()));
        }
        Ok(())
    }

    /// The full chat-completions endpoint.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn system_prompt(&self) -> &str {
        self.system_prompt.as_deref().unwrap_or(SYSTEM_PROMPT)
    }
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Output guards
// ---------------------------------------------------------------------------

/// Structural checks applied to model output before it reaches the user.
///
/// These do not judge quality — no cheap check can. They detect the shapes a
/// *rewrite* has: text that grew (content added), text that collapsed (content
/// dropped), or a number that changed. Any of those means the model did
/// something other than clean up, and the safe response is to discard its answer
/// and keep the rule engine's.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct OutputGuards {
    /// Longest the output may be, as a multiple of the input.
    pub max_growth_ratio: f32,
    /// Shortest the output may be, as a fraction of the input.
    pub min_shrink_ratio: f32,
    /// Extra characters always allowed on top of `max_growth_ratio`, so that
    /// punctuating a very short utterance does not trip the ratio.
    pub growth_slack: usize,
    /// Require every digit run in the input to survive into the output.
    pub require_digit_preservation: bool,
}

impl Default for OutputGuards {
    fn default() -> Self {
        Self {
            // Cleanup only ever removes words, so growth beyond punctuation is
            // already suspicious; 1.5x leaves room for casing and spacing fixes.
            max_growth_ratio: 1.5,
            // Removing every disfluency from a very messy transcript can plausibly
            // cost a third of its characters. Below 40% something else happened.
            min_shrink_ratio: 0.4,
            growth_slack: 32,
            require_digit_preservation: true,
        }
    }
}

impl OutputGuards {
    /// Guards that accept anything. For tests and for callers who trust their model.
    pub fn permissive() -> Self {
        Self {
            max_growth_ratio: f32::INFINITY,
            min_shrink_ratio: 0.0,
            growth_slack: usize::MAX,
            require_digit_preservation: false,
        }
    }

    /// Check `output` against `input`, naming the first violation.
    pub fn check(&self, input: &str, output: &str) -> Result<(), PolishError> {
        if output.trim().is_empty() {
            return Err(PolishError::Rejected("model returned empty text".into()));
        }

        let input_len = input.trim().chars().count();
        let output_len = output.trim().chars().count();

        if input_len > 0 {
            let ceiling = (input_len as f32 * self.max_growth_ratio) as usize;
            let ceiling = ceiling.max(input_len.saturating_add(self.growth_slack));
            if output_len > ceiling {
                return Err(PolishError::Rejected(format!(
                    "output grew from {input_len} to {output_len} characters, which means content was added"
                )));
            }

            let floor = (input_len as f32 * self.min_shrink_ratio) as usize;
            if output_len < floor {
                return Err(PolishError::Rejected(format!(
                    "output shrank from {input_len} to {output_len} characters, which means content was dropped"
                )));
            }
        }

        if self.require_digit_preservation {
            for run in digit_runs(input) {
                if !output.contains(&run) {
                    return Err(PolishError::Rejected(format!(
                        "the number {run:?} did not survive into the output"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Every maximal run of digits in `text`.
fn digit_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Undo the wrappers a chat model puts around an answer.
///
/// Models are trained to be conversational; even under an explicit
/// "output only the text" instruction they occasionally return a fenced block or
/// a "Here's the cleaned text:" preamble. Stripping those is strictly safer than
/// letting the guards reject the whole response over formatting.
fn sanitize_output(raw: &str, input: &str) -> String {
    let mut text = raw.trim().to_string();

    // ```lang\n...\n```
    if text.starts_with("```") {
        if let Some(rest) = text.strip_prefix("```") {
            let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
            if let Some(inner) = body.rsplit_once("```") {
                text = inner.0.trim().to_string();
            }
        }
    }

    // "Here is the cleaned text:\n..."
    const PREAMBLES: &[&str] = &[
        "here is the cleaned text:",
        "here's the cleaned text:",
        "here is the cleaned version:",
        "here's the cleaned version:",
        "cleaned text:",
        "cleaned:",
        "output:",
        "result:",
    ];
    let lowered = text.to_lowercase();
    for preamble in PREAMBLES {
        if lowered.starts_with(preamble) {
            let rest = text[preamble.len()..].trim_start();
            if !rest.is_empty() {
                text = rest.to_string();
            }
            break;
        }
    }

    // Surrounding quotes the input did not have.
    let quoted = (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\u{201c}') && text.ends_with('\u{201d}'));
    let input_quoted = input.trim().starts_with('"') || input.trim().starts_with('\u{201c}');
    if quoted && !input_quoted && text.chars().count() > 1 {
        let mut chars = text.chars();
        chars.next();
        chars.next_back();
        let inner: String = chars.collect();
        if !inner.contains('"') {
            text = inner;
        }
    }

    text.trim().to_string()
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The future returned by [`ChatTransport::post_json`].
pub type TransportFuture<'a> = Pin<Box<dyn Future<Output = Result<String, PolishError>> + Send + 'a>>;

/// The HTTP layer, behind a trait so tests never touch a socket.
///
/// It deals in strings rather than typed bodies on purpose: a test double can
/// then return a byte-for-byte real API response, so response parsing is
/// exercised too, not stubbed over.
pub trait ChatTransport: Send + Sync + fmt::Debug {
    /// POST `body` as `application/json` with a bearer token; return the response body.
    fn post_json<'a>(&'a self, url: &'a str, api_key: &'a str, body: &'a str)
        -> TransportFuture<'a>;

    /// Open a connection ahead of time.
    ///
    /// Worth calling when Iris starts recording: a cold TLS handshake costs more
    /// than the entire polish budget, so the first dictation of a session would
    /// otherwise always fall back.
    fn warm_up<'a>(&'a self, _url: &'a str) -> TransportFuture<'a> {
        Box::pin(async { Ok(String::new()) })
    }
}

/// The production transport.
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a client tuned for one short request on a latency budget.
    pub fn new(timeout: Duration) -> Result<Self, PolishError> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Nagle would add up to 40 ms to a request that fits in one segment.
            .tcp_nodelay(true)
            // Keep the connection hot: re-handshaking per utterance is fatal here.
            .pool_idle_timeout(Duration::from_secs(300))
            .pool_max_idle_per_host(2)
            .user_agent(concat!("iris-polish/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| PolishError::Config(format!("could not build HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

/// rustls needs a process-wide crypto provider. We pick `ring` rather than
/// `aws-lc-rs` because it cross-compiles to `x86_64-pc-windows-gnu` without
/// cmake or NASM, and Iris is Windows-first.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err just means another crate installed one first, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl ChatTransport for ReqwestTransport {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        api_key: &'a str,
        body: &'a str,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .post(url)
                .bearer_auth(api_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| PolishError::Transport(e.to_string()))?;

            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| PolishError::Transport(format!("could not read response body: {e}")))?;

            if !status.is_success() {
                return Err(PolishError::HttpStatus {
                    status: status.as_u16(),
                    body: truncate(&text, 400),
                });
            }
            Ok(text)
        })
    }

    fn warm_up<'a>(&'a self, url: &'a str) -> TransportFuture<'a> {
        Box::pin(async move {
            let _ = self.client.get(url).send().await;
            Ok(String::new())
        })
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}...")
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Bound the model's output length to roughly the input's.
///
/// This is a latency control as much as a safety one: generation time scales
/// with tokens emitted, and a runaway completion would blow the budget even on a
/// fast endpoint.
fn max_tokens_for(input: &str) -> u32 {
    let estimate = input.chars().count() / 3 + 64;
    estimate.clamp(64, 2048) as u32
}

/// Pull the assistant message out of an OpenAI-compatible response.
fn extract_content(body: &str) -> Result<String, PolishError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| PolishError::Response(format!("response was not JSON: {e}")))?;

    if let Some(message) = value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(PolishError::Response(format!("endpoint reported: {message}")));
    }

    let content = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            PolishError::Response(format!(
                "no choices[0].message.content in response: {}",
                truncate(body, 200)
            ))
        })?;

    Ok(content.to_string())
}

// ---------------------------------------------------------------------------
// The polisher
// ---------------------------------------------------------------------------

/// Transcript cleanup via a chat-completions endpoint.
///
/// ```no_run
/// use iris_polish::{LlmPolisher, PolishRequest, Polisher};
///
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let polisher = LlmPolisher::from_env()?;   // reads IRIS_GROQ_KEY
/// polisher.warm_up().await;                  // pay the TLS cost before the user speaks
/// let out = polisher.polish(&PolishRequest::new("um so uh it works")).await?;
/// assert_eq!(out.text, "So it works.");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct LlmPolisher {
    config: LlmConfig,
    endpoint: String,
    transport: Arc<dyn ChatTransport>,
}

impl LlmPolisher {
    /// Build from the environment; see [`LlmConfig::from_env`].
    pub fn from_env() -> Result<Self, PolishError> {
        Self::new(LlmConfig::from_env()?)
    }

    /// Build with an explicit configuration and the real HTTP transport.
    pub fn new(config: LlmConfig) -> Result<Self, PolishError> {
        config.validate()?;
        let transport = ReqwestTransport::new(config.timeout)?;
        Ok(Self::with_transport(config, Arc::new(transport)))
    }

    /// Build with a supplied transport. This is how the tests stay offline.
    pub fn with_transport(config: LlmConfig, transport: Arc<dyn ChatTransport>) -> Self {
        Self {
            endpoint: config.endpoint(),
            config,
            transport,
        }
    }

    /// The configuration in force.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Open the connection before it is needed. Errors are deliberately ignored:
    /// a failed warm-up is not a reason to tell the user anything.
    pub async fn warm_up(&self) {
        let _ = self.transport.warm_up(&self.config.base_url).await;
    }

    /// Serialise the chat-completions request body for `request`.
    ///
    /// Public so callers can log or snapshot exactly what goes on the wire.
    pub fn request_body(&self, request: &PolishRequest) -> Result<String, PolishError> {
        let user = build_user_message(request);
        let body = ChatRequest {
            model: &self.config.model,
            messages: [
                ChatMessage {
                    role: "system",
                    content: self.config.system_prompt(),
                },
                ChatMessage {
                    role: "user",
                    content: &user,
                },
            ],
            temperature: self.config.temperature,
            max_tokens: max_tokens_for(&request.text),
            stream: false,
        };
        serde_json::to_string(&body)
            .map_err(|e| PolishError::Config(format!("could not serialise request: {e}")))
    }
}

impl Polisher for LlmPolisher {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();

            // Nothing to say: do not spend a round trip on it.
            if request.text.trim().is_empty() {
                return Ok(Polished::new(
                    request.text.clone(),
                    PolishSource::Unchanged,
                    started.elapsed(),
                ));
            }

            let body = self.request_body(request)?;
            let call = self
                .transport
                .post_json(&self.endpoint, &self.config.api_key, &body);

            let response = match tokio::time::timeout(self.config.timeout, call).await {
                Ok(result) => result?,
                Err(_) => {
                    let budget_ms = self.config.timeout.as_millis() as u64;
                    tracing::debug!(budget_ms, "llm polish exceeded its budget");
                    return Err(PolishError::BudgetExceeded { budget_ms });
                }
            };

            let content = extract_content(&response)?;
            let text = sanitize_output(&content, &request.text);
            self.config.guards.check(&request.text, &text)?;

            let elapsed = started.elapsed();
            tracing::debug!(elapsed_ms = elapsed.as_millis() as u64, "llm polish");
            Ok(Polished::new(text, PolishSource::Llm, elapsed))
        })
    }

    fn expected_latency(&self) -> Option<Duration> {
        Some(self.config.timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_built_without_double_slashes() {
        let config = LlmConfig::new("k").with_base_url("https://api.groq.com/openai/v1/");
        assert_eq!(
            config.endpoint(),
            "https://api.groq.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn defaults_point_at_groq() {
        let config = LlmConfig::new("k");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.temperature, 0.0);
        assert_eq!(config.timeout, crate::DEFAULT_LATENCY_BUDGET);
    }

    #[test]
    fn validation_rejects_unusable_configurations() {
        assert!(matches!(
            LlmConfig::new("").validate(),
            Err(PolishError::MissingApiKey)
        ));
        assert!(matches!(
            LlmConfig::new("k").with_model("").validate(),
            Err(PolishError::Config(_))
        ));
        assert!(matches!(
            LlmConfig::new("k").with_base_url("api.groq.com").validate(),
            Err(PolishError::Config(_))
        ));
        assert!(matches!(
            LlmConfig::new("k")
                .with_timeout(Duration::ZERO)
                .validate(),
            Err(PolishError::Config(_))
        ));
        assert!(LlmConfig::new("k").validate().is_ok());
    }

    #[test]
    fn digit_runs_are_found() {
        assert_eq!(digit_runs("at 5 pm on the 23rd, room 1a"), ["5", "23", "1"]);
        assert!(digit_runs("no numbers here").is_empty());
    }

    #[test]
    fn guards_reject_growth() {
        let guards = OutputGuards::default();
        let input = "this is a reasonably long transcript about the build system";
        let grown = format!("{input} and here is some extra commentary the model invented");
        assert!(matches!(
            guards.check(input, &grown),
            Err(PolishError::Rejected(_))
        ));
    }

    #[test]
    fn guards_allow_punctuating_a_short_utterance() {
        let guards = OutputGuards::default();
        assert!(guards.check("ok", "OK.").is_ok());
        assert!(guards.check("um yes", "Yes.").is_ok());
    }

    #[test]
    fn guards_reject_collapse() {
        let guards = OutputGuards::default();
        let input = "we should ship the fix today because the release is tomorrow morning";
        assert!(matches!(
            guards.check(input, "Ship it."),
            Err(PolishError::Rejected(_))
        ));
    }

    #[test]
    fn guards_reject_a_lost_number() {
        let guards = OutputGuards::default();
        assert!(matches!(
            guards.check("the meeting is at 4 pm", "The meeting is at four pm."),
            Err(PolishError::Rejected(_))
        ));
        assert!(guards
            .check("the meeting is at 4 pm", "The meeting is at 4 pm.")
            .is_ok());
    }

    #[test]
    fn guards_reject_empty_output() {
        assert!(matches!(
            OutputGuards::default().check("something", "   "),
            Err(PolishError::Rejected(_))
        ));
    }

    #[test]
    fn permissive_guards_accept_anything() {
        let guards = OutputGuards::permissive();
        assert!(guards.check("hi", "an entirely different and much longer sentence 7").is_ok());
    }

    #[test]
    fn sanitizer_strips_code_fences() {
        assert_eq!(sanitize_output("```\nHello there.\n```", "hello there"), "Hello there.");
        assert_eq!(
            sanitize_output("```text\nHello there.\n```", "hello there"),
            "Hello there."
        );
    }

    #[test]
    fn sanitizer_strips_preambles() {
        assert_eq!(
            sanitize_output("Here is the cleaned text:\nHello there.", "hello there"),
            "Hello there."
        );
        assert_eq!(sanitize_output("Output: Hello there.", "hello there"), "Hello there.");
    }

    #[test]
    fn sanitizer_strips_added_quotes_but_keeps_dictated_ones() {
        assert_eq!(sanitize_output("\"Hello there.\"", "hello there"), "Hello there.");
        assert_eq!(
            sanitize_output("\"Hello there.\"", "\"hello there\""),
            "\"Hello there.\""
        );
        // Interior quotes mean the outer ones are probably real.
        assert_eq!(
            sanitize_output("\"he said \"hi\"\"", "he said hi"),
            "\"he said \"hi\"\""
        );
    }

    #[test]
    fn sanitizer_leaves_clean_output_alone() {
        assert_eq!(sanitize_output("Hello there.", "hello there"), "Hello there.");
    }

    #[test]
    fn extracts_content_from_a_real_shaped_response() {
        let body = r#"{"id":"x","object":"chat.completion","choices":[
            {"index":0,"message":{"role":"assistant","content":"Hello there."},"finish_reason":"stop"}
        ],"usage":{"total_tokens":10}}"#;
        assert_eq!(extract_content(body).unwrap(), "Hello there.");
    }

    #[test]
    fn extraction_surfaces_endpoint_errors() {
        let body = r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        let err = extract_content(body).unwrap_err();
        assert!(err.to_string().contains("model not found"), "{err}");
    }

    #[test]
    fn extraction_rejects_malformed_bodies() {
        assert!(matches!(
            extract_content("not json at all"),
            Err(PolishError::Response(_))
        ));
        assert!(matches!(
            extract_content(r#"{"choices":[]}"#),
            Err(PolishError::Response(_))
        ));
        assert!(matches!(
            extract_content(r#"{"choices":[{"message":{}}]}"#),
            Err(PolishError::Response(_))
        ));
    }

    #[test]
    fn max_tokens_tracks_input_size_within_bounds() {
        assert_eq!(max_tokens_for(""), 64);
        assert!(max_tokens_for(&"a".repeat(300)) > 64);
        assert_eq!(max_tokens_for(&"a".repeat(100_000)), 2048);
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("\u{e9}\u{e9}\u{e9}\u{e9}", 2), "\u{e9}\u{e9}...");
    }
}
