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

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;

use crate::{audio, vlog};

use super::{net, Engine, EngineOptions, Session, TranscriptEvent};

const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";
const DEFAULT_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// See [`GroqEngine::final_timeout`]. `transcribe()` sets no request timeout of
/// its own, so this is the only bound on the whole round trip.
const FINAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct GroqEngine {
    key: String,
    model: String,
    url: String,
    language: Option<String>,
}

/// Hand-written so the API key cannot reach a log line, a panic message or a
/// bug report through a stray `{:?}`.
impl std::fmt::Debug for GroqEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroqEngine")
            .field("model", &self.model)
            .field("url", &self.url)
            .field("language", &self.language)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl GroqEngine {
    pub fn from_env(opts: &EngineOptions) -> Result<Self> {
        let key = super::require_key("IRIS_GROQ_KEY", "groq", "https://console.groq.com/keys")?;
        Ok(Self {
            key,
            model: opts
                .model
                .clone()
                .or_else(|| std::env::var("IRIS_GROQ_MODEL").ok())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            url: std::env::var("IRIS_GROQ_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            language: opts.language.clone(),
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

    /// Provisional, and deliberately generous: WAV encode, upload of the whole
    /// utterance and Whisper inference all happen after key-up, and none of it
    /// has ever been live-measured in this project (`docs/spike-findings.md`
    /// lists this engine as compile + unit-tested, not run). A minute of speech
    /// is a ~1.9 MB upload, which a weak uplink alone can spend the streaming
    /// budget on, and `streams_partials` is `false` here — expiry loses the
    /// whole transcript, not a tail of it. Tighten this only against real
    /// measured Groq latency, never by analogy with the streaming default.
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
        let tx = self.events_tx.clone();

        net::runtime()?.spawn(async move {
            match transcribe(url, key, model, language, wav).await {
                Ok(text) if !text.trim().is_empty() => {
                    let _ = tx.send(TranscriptEvent::Final(text.trim().to_string()));
                }
                Ok(_) => {
                    let _ = tx.send(TranscriptEvent::Error(
                        "Groq returned an empty transcript".into(),
                    ));
                }
                Err(e) => {
                    let _ = tx.send(TranscriptEvent::Error(format!("{e:#}")));
                }
            }
        });
        Ok(())
    }
}

async fn transcribe(
    url: String,
    key: String,
    model: String,
    language: Option<String>,
    wav: Vec<u8>,
) -> Result<String> {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let mut form = reqwest::multipart::Form::new()
        .text("model", model)
        .text("response_format", "json")
        .text("temperature", "0")
        .part("file", part);
    if let Some(lang) = language {
        form = form.text("language", lang);
    }

    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .context("posting audio to Groq")?;

    let status = response.status();
    let body = response.text().await.context("reading the Groq response")?;
    if !status.is_success() {
        anyhow::bail!("Groq returned HTTP {status}: {}", body.trim());
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
        };
        assert!(!engine.streams_partials());
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
        };
        assert!(
            engine.final_timeout() > crate::dictation::DEFAULT_FINAL_TIMEOUT,
            "a batch engine needs more room than the streaming default, not less: {:?}",
            engine.final_timeout()
        );
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
}
