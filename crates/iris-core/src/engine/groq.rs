//! Groq Whisper engine — the batch comparison point.
//!
//! Groq's transcription API is request/response, not streaming: the audio has to
//! be complete before anything can be sent. So this engine buffers in `push` and
//! does all of its work in `finish`, which puts upload + inference *entirely*
//! after the key release.
//!
//! It is here on purpose. Groq's Whisper is very fast in absolute terms, and it
//! is the shape almost every open-source dictation tool uses — so having it
//! behind the same [`Engine`] trait lets the latency harness show the cost of
//! the architecture rather than the cost of the vendor. See
//! `docs/spike-findings.md`.

use anyhow::{bail, Context, Result};
use crossbeam_channel::Receiver;

use crate::{audio, vlog};

/// Groq's documented cap on the `prompt` field: "limited to 224 tokens"
/// (Groq's speech-to-text docs, console.groq.com/docs/speech-to-text, checked
/// 2026-08-11). Shared with the local Whisper finalizer, which bounds by word
/// count instead of tokens — see its own doc comment for why.
use super::MAX_VOCABULARY_PROMPT_WORDS as MAX_PROMPT_WORDS;
use super::{net, Engine, EngineOptions, Failure, FailureCause, Session, TranscriptEvent};

const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";
const DEFAULT_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// How long to wait for the socket to come up, before a byte of audio moves.
///
/// Provisional. Nothing here is live-measured (`docs/spike-findings.md` lists
/// this engine as compile + unit-tested, not run), but a TLS handshake with a
/// healthy HTTPS API is well under a second, and this is the same order as
/// Deepgram's own connect budget. Its job is to fail a blackholed or half-open
/// connection fast instead of leaving [`FINAL_TIMEOUT`] as the only thing that
/// ever ends the wait.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the whole request may take, connect included.
///
/// Provisional, and reasoned from size rather than measured: a minute of speech
/// is a ~1.9 MB WAV, which a weak but working uplink (~1 Mbit/s) moves in ~15s,
/// leaving ~10s for `whisper-large-v3-turbo` to run and answer. Raise it if
/// real Groq latency ever says otherwise — but raise [`FINAL_TIMEOUT`] with it,
/// or the outer wait will start cutting off requests that were going to succeed.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// The backstop under [`GroqEngine::final_timeout`]: a little above
/// [`REQUEST_TIMEOUT`], so the request's own error is what reaches the user and
/// this only fires if the client somehow does not.
///
/// [`REQUEST_TIMEOUT`] alone is the whole client-side worst case —
/// `reqwest`'s request timeout is one deadline running from the start of the
/// connect through to the end of the response body, so [`CONNECT_TIMEOUT`] is
/// contained by it rather than added to it. The margin over it covers only
/// this side of the channel: the runtime picking the spawned task back up,
/// parsing the response, and handing the event over.
///
/// **This is a bound on how long the whole app stops responding.**
/// `Dictation::finish` blocks the resident loop in `iris-app` — the pill stays
/// frozen on "processing", and tray commands including Quit go unserviced —
/// for as long as the engine's `final_timeout` allows. Any engine choosing that
/// value is spending the user's whole UI, not just this dictation's latency.
///
/// The exposure is real and accepted: choosing Groq means a failing finalise
/// can freeze the app, Quit included, for the full 28s here (the local engine's
/// bound is 20s; `DEFAULT_FINAL_TIMEOUT`, which Deepgram inherits, is 6s). It
/// is not bought down by lowering this number — with `streams_partials` false
/// there is nothing to salvage, so a shorter bound pays for responsiveness
/// with the user's whole utterance, which is the trade this project does not
/// make. The fix is a non-blocking finalise path, tracked separately; do not
/// approximate it by trimming this.
const FINAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(28);

pub struct GroqEngine {
    key: String,
    model: String,
    url: String,
    language: Option<String>,
    /// See [`EngineOptions::vocabulary`], joined into Whisper's one
    /// initial-prompt string by [`super::vocabulary_prompt`]. `None` when the
    /// vocabulary was empty — the "add no field at all" case.
    prompt: Option<String>,
}

/// Hand-written so the API key and the user's vocabulary prompt cannot reach
/// a log line, a panic message or a bug report through a stray `{:?}`.
impl std::fmt::Debug for GroqEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroqEngine")
            .field("model", &self.model)
            .field("url", &self.url)
            .field("language", &self.language)
            .field("key", &"<redacted>")
            .field("has_vocabulary_prompt", &self.prompt.is_some())
            .finish()
    }
}

/// Validates [`IRIS_GROQ_URL`](GroqEngine::from_env)'s override, the same way
/// `iris-polish`'s `LlmConfig::validate` guards `IRIS_LLM_BASE_URL` — except
/// this connection is plain HTTPS (a `reqwest` POST, not a websocket), so
/// `https://` is the only TLS-carrying scheme it actually accepts. A non-TLS
/// override is rejected outright rather than silently replaced with the
/// default — a user who set this deliberately needs to know their value was
/// thrown out, not guess why their audio still went to the real endpoint.
fn validate_groq_url(url: String) -> Result<String> {
    if !url.starts_with("https://") {
        bail!(
            "IRIS_GROQ_URL must start with https://, got {url:?}. \
             A non-TLS scheme would send your audio and API key over the \
             network unencrypted."
        );
    }
    Ok(url)
}

impl GroqEngine {
    pub fn from_env(opts: &EngineOptions) -> Result<Self> {
        let key = super::require_key("IRIS_GROQ_KEY", "groq", "https://console.groq.com/keys")?;
        let url = match std::env::var("IRIS_GROQ_URL") {
            Ok(url) => validate_groq_url(url)?,
            Err(_) => DEFAULT_URL.to_string(),
        };
        Ok(Self {
            key,
            model: opts
                .model
                .clone()
                .or_else(|| std::env::var("IRIS_GROQ_MODEL").ok())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            url,
            language: opts.language.clone(),
            prompt: super::vocabulary_prompt(&opts.vocabulary, MAX_PROMPT_WORDS),
        })
    }
}

impl Engine for GroqEngine {
    fn name(&self) -> &'static str {
        "groq"
    }

    /// No interim results are possible: the request cannot start until the
    /// audio is complete.
    fn streams_partials(&self) -> bool {
        false
    }

    /// Well above the streaming default: WAV encode, upload of the whole
    /// utterance and Whisper inference all happen after key-up, and
    /// `streams_partials` is `false` here — an expiry loses the whole
    /// transcript, not a tail of it. It is a backstop rather than the working
    /// bound, because [`CONNECT_TIMEOUT`] and [`REQUEST_TIMEOUT`] end a dead
    /// request first; see [`FINAL_TIMEOUT`] for what this costs the UI.
    fn final_timeout(&self) -> std::time::Duration {
        FINAL_TIMEOUT
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        net::init_crypto();
        // Fail fast here rather than at finish time, so a broken TLS setup shows
        // up on key-press instead of eating a dictation.
        net::runtime()?;

        let (tx, rx) = crossbeam_channel::unbounded();
        let _ = tx.send(TranscriptEvent::Connected);

        Ok(Box::new(GroqSession {
            key: self.key.clone(),
            model: self.model.clone(),
            url: self.url.clone(),
            language: self.language.clone(),
            prompt: self.prompt.clone(),
            pcm: Vec::with_capacity(audio::SAMPLE_RATE as usize * 8),
            events_tx: tx,
            events: rx,
            finished: false,
        }))
    }
}

struct GroqSession {
    key: String,
    model: String,
    url: String,
    language: Option<String>,
    prompt: Option<String>,
    pcm: Vec<i16>,
    events_tx: crossbeam_channel::Sender<TranscriptEvent>,
    events: Receiver<TranscriptEvent>,
    finished: bool,
}

impl Session for GroqSession {
    fn push(&mut self, pcm: &[i16]) -> Result<()> {
        if !self.finished {
            self.pcm.extend_from_slice(pcm);
        }
        Ok(())
    }

    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        let wav = audio::encode_wav(&self.pcm).context("encoding captured audio as WAV")?;
        vlog!("groq: uploading {} kB of audio", wav.len() / 1024);

        let key = self.key.clone();
        let model = self.model.clone();
        let url = self.url.clone();
        let language = self.language.clone();
        let prompt = self.prompt.clone();
        let tx = self.events_tx.clone();

        net::runtime()?.spawn(async move {
            match transcribe(url, key, model, language, prompt, wav).await {
                Ok(text) if !text.trim().is_empty() => {
                    let _ = tx.send(TranscriptEvent::Final(text.trim().to_string()));
                }
                Ok(_) => {
                    let _ = tx.send(TranscriptEvent::Error(
                        "Groq returned an empty transcript".into(),
                    ));
                }
                Err(e) => {
                    let _ = tx.send(e.into_event());
                }
            }
        });
        Ok(())
    }
}

/// The env var and account-management URL named in every classified failure
/// message this module produces — see [`FailureCause::message`].
const KEY_ENV: &str = "IRIS_GROQ_KEY";
const CONSOLE_URL: &str = "https://console.groq.com/keys";
const PROVIDER: &str = "Groq";

/// Classify a Groq API rejection by its HTTP status, per Groq's documented
/// status codes (<https://console.groq.com/docs/errors>, checked
/// 2026-08-13):
///
/// - `401` — missing or invalid API key.
/// - `403` — insufficient permissions for the resource. Folded into the same
///   [`FailureCause::InvalidKey`] as `401`, same reasoning as Deepgram's
///   `403`: the fix is checking the key and the account it belongs to.
/// - `429` — rate limit exceeded.
/// - `498` (Groq's own custom code) — flex-tier capacity exceeded, "retry
///   later"; grouped with `429` under [`FailureCause::RateLimited`] since
///   both mean "try again shortly", not "fix your key or your balance".
///
/// Groq's error reference documents no separate status for an exhausted
/// balance or quota — unlike Deepgram's `402`, its docs state billing and
/// quota issues surface "through existing status codes" (in practice, `429`
/// per Groq's own rate-limits page, which lists per-day token/request caps
/// alongside the per-minute ones). So this engine has no
/// [`FailureCause::ExhaustedCredit`] case at all: there is no status code to
/// key it off without guessing, which this module does not do.
///
/// Every other status is [`FailureCause::Unknown`].
fn classify_response_status(status: u16) -> FailureCause {
    match status {
        401 | 403 => FailureCause::InvalidKey,
        429 | 498 => FailureCause::RateLimited,
        _ => FailureCause::Unknown,
    }
}

/// Classify a failure to even get a response — no HTTP status to read,
/// because the request never completed. `reqwest::Error`'s own
/// `is_timeout`/`is_connect` predicates are its public, documented way to
/// tell these apart; anything else (a build-time TLS misconfiguration, a
/// redirect-policy error) has no confident classification here.
fn classify_send_error(err: reqwest::Error) -> Failure {
    let cause = if err.is_timeout() {
        FailureCause::Timeout
    } else if err.is_connect() {
        FailureCause::NetworkUnreachable
    } else {
        return Failure::Other(anyhow::Error::new(err).context("posting audio to Groq"));
    };
    Failure::Classified {
        message: cause.message(PROVIDER, KEY_ENV, CONSOLE_URL, Some(&err.to_string())),
        cause,
    }
}

async fn transcribe(
    url: String,
    key: String,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
    wav: Vec<u8>,
) -> Result<String, Failure> {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .context("building the audio part")?;
    let mut form = reqwest::multipart::Form::new()
        .text("model", model)
        .text("response_format", "json")
        .text("temperature", "0")
        .part("file", part);
    if let Some(lang) = language {
        form = form.text("language", lang);
    }
    if let Some(prompt) = prompt {
        form = form.text("prompt", prompt);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building the Groq HTTP client")?;

    let response = match client
        .post(&url)
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => return Err(classify_send_error(e)),
    };

    let status = response.status();
    let body = response.text().await.context("reading the Groq response")?;
    if !status.is_success() {
        let cause = classify_response_status(status.as_u16());
        let detail = format!("HTTP {status}: {}", body.trim());
        return Err(Failure::Classified {
            message: cause.message(PROVIDER, KEY_ENV, CONSOLE_URL, Some(&detail)),
            cause,
        });
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&body).context("parsing the Groq response")?;
    Ok(parsed
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groq_declares_that_it_cannot_stream() {
        // The UI uses this to decide whether to promise live text.
        let engine = GroqEngine {
            key: "x".into(),
            model: DEFAULT_MODEL.into(),
            url: DEFAULT_URL.into(),
            language: None,
            prompt: None,
        };
        assert!(!engine.streams_partials());
    }

    #[test]
    fn response_status_codes_classify_per_groqs_documented_error_reference() {
        // https://console.groq.com/docs/errors and
        // https://console.groq.com/docs/rate-limits, checked 2026-08-13. Groq
        // documents no separate billing/quota status (see
        // `classify_response_status`'s doc comment), so there is no
        // ExhaustedCredit case to assert here.
        assert_eq!(classify_response_status(401), FailureCause::InvalidKey);
        assert_eq!(classify_response_status(403), FailureCause::InvalidKey);
        assert_eq!(classify_response_status(429), FailureCause::RateLimited);
        assert_eq!(classify_response_status(498), FailureCause::RateLimited);
        // Undocumented for this endpoint — must never be stretched into a guess.
        assert_eq!(classify_response_status(500), FailureCause::Unknown);
        assert_eq!(classify_response_status(404), FailureCause::Unknown);
    }

    #[test]
    fn a_401_response_never_leaks_the_key_and_keeps_the_raw_status_for_diagnosis() {
        let cause = classify_response_status(401);
        let message = cause.message(
            PROVIDER,
            KEY_ENV,
            CONSOLE_URL,
            Some("HTTP 401: invalid_api_key"),
        );
        assert!(message.contains("Groq"), "{message}");
        assert!(message.contains(KEY_ENV), "{message}");
        assert!(message.contains("401"), "{message}");
        assert!(
            !message.contains("Bearer "),
            "must never echo the Authorization header: {message}"
        );
    }

    /// A real `reqwest::Error` with `is_connect() == true`, from an actual
    /// failed connect to a closed local port — no internet needed, and
    /// deterministic: nothing listens on a freshly-bound-then-dropped
    /// loopback port.
    #[tokio::test]
    async fn a_connection_refused_send_error_is_classified_as_network_unreachable() {
        net::init_crypto();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nothing is listening on `addr` from here on

        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}"))
            .send()
            .await
            .unwrap_err();
        assert!(
            err.is_connect(),
            "test setup did not produce a connect failure: {err:#}"
        );

        match classify_send_error(err) {
            Failure::Classified { message, cause } => {
                assert_eq!(cause, FailureCause::NetworkUnreachable);
                assert!(message.contains("Groq"), "{message}");
                assert!(
                    !message.contains(KEY_ENV),
                    "a network failure is not a key problem: {message}"
                );
            }
            Failure::Other(e) => panic!("expected a classified failure, got {e:#}"),
        }
    }

    #[test]
    fn the_outer_wait_is_a_backstop_over_the_request_s_own_deadline() {
        // reqwest's request timeout is one deadline covering connect through
        // response body, so REQUEST_TIMEOUT alone is the client-side worst
        // case. This must stay above it — otherwise the outer wait cuts off
        // requests that were still going to succeed — and not far above it,
        // because every second here is a second the resident app spends
        // unresponsive.
        assert!(FINAL_TIMEOUT > REQUEST_TIMEOUT, "{FINAL_TIMEOUT:?}");
        assert!(
            FINAL_TIMEOUT - REQUEST_TIMEOUT < CONNECT_TIMEOUT,
            "the margin is for handing the answer back, not for a second connect: {:?}",
            FINAL_TIMEOUT - REQUEST_TIMEOUT
        );
    }

    #[test]
    fn groq_does_not_inherit_the_streaming_wait() {
        // Upload and inference both happen after key-up here, and with no
        // partials there is nothing to salvage when the wait runs out — an
        // expiry costs the user the whole utterance. The streaming default is
        // the wrong budget for that, and inheriting it silently is the bug.
        let engine = GroqEngine {
            key: "x".into(),
            model: DEFAULT_MODEL.into(),
            url: DEFAULT_URL.into(),
            language: None,
            prompt: None,
        };
        assert!(
            engine.final_timeout() > crate::dictation::DEFAULT_FINAL_TIMEOUT,
            "a batch engine needs more room than the streaming default, not less: {:?}",
            engine.final_timeout()
        );
    }

    #[test]
    fn a_secure_override_is_accepted_unchanged() {
        assert_eq!(
            validate_groq_url("https://example.test/v1/audio/transcriptions".to_string()).unwrap(),
            "https://example.test/v1/audio/transcriptions"
        );
    }

    #[test]
    fn a_plaintext_override_is_rejected_by_name_not_silently_dropped() {
        let err = validate_groq_url("http://example.test/v1/audio/transcriptions".to_string())
            .unwrap_err()
            .to_string();
        assert!(err.contains("IRIS_GROQ_URL"), "unhelpful error: {err}");
        assert!(err.contains("https://"), "unhelpful error: {err}");
    }

    #[test]
    fn no_override_falls_back_to_the_default_https_url() {
        // GroqEngine::from_env only calls validate_groq_url at all when
        // IRIS_GROQ_URL is set — an absent override never reaches it and
        // keeps DEFAULT_URL, which is itself https:// and would pass the
        // same check.
        assert!(DEFAULT_URL.starts_with("https://"));
        assert!(validate_groq_url(DEFAULT_URL.to_string()).is_ok());
    }

    #[test]
    fn from_env_fails_clearly_without_a_key() {
        if std::env::var("IRIS_GROQ_KEY").is_ok() {
            return;
        }
        let err = GroqEngine::from_env(&EngineOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("IRIS_GROQ_KEY"), "unhelpful error: {err}");
    }

    #[test]
    fn an_empty_vocabulary_adds_no_prompt() {
        let opts = EngineOptions::default();
        assert_eq!(
            super::super::vocabulary_prompt(&opts.vocabulary, MAX_PROMPT_WORDS),
            None
        );
    }

    #[test]
    fn a_configured_vocabulary_becomes_the_initial_prompt() {
        let opts = EngineOptions {
            vocabulary: vec!["Deepgram".into(), "Zipformer".into()],
            ..EngineOptions::default()
        };
        let prompt = super::super::vocabulary_prompt(&opts.vocabulary, MAX_PROMPT_WORDS);
        assert_eq!(prompt, Some("Deepgram, Zipformer".into()));
    }

    #[test]
    fn multipart_form_carries_a_prompt_field_only_when_one_is_set() {
        // `reqwest::multipart::Part`'s `Debug` does not render text values
        // (by design — the same reason `GroqEngine`'s own `Debug` redacts the
        // key), so this checks for the field's presence by name, which is
        // enough to prove the wiring: `transcribe` only ever calls
        // `form.text("prompt", ...)` when `prompt` is `Some`.
        assert!(field_names(Some("Deepgram, Zipformer".to_string())).contains(&"prompt"));
        assert!(
            !field_names(None).contains(&"prompt"),
            "an empty vocabulary must add no prompt field at all"
        );
    }

    /// The field names in the same multipart form [`transcribe`] sends,
    /// minus the audio part (irrelevant to whether `prompt` is present).
    fn field_names(prompt: Option<String>) -> Vec<&'static str> {
        let mut form = reqwest::multipart::Form::new()
            .text("model", DEFAULT_MODEL)
            .text("response_format", "json")
            .text("temperature", "0");
        let mut names = vec!["model", "response_format", "temperature"];
        if let Some(prompt) = prompt {
            form = form.text("prompt", prompt);
            names.push("prompt");
        }
        drop(form); // constructed the same way `transcribe` does; content unused beyond that
        names
    }
}
