//! Deepgram streaming websocket engine — the fast path.
//!
//! Audio goes up as raw 16 kHz linear PCM the moment it is captured, and
//! Deepgram returns interim hypotheses continuously. By the time the user lets
//! go of the hotkey, everything but the last fragment of speech has *usually*
//! already been transcribed, so `finish()` costs a Finalize + CloseStream flush
//! rather than a whole inference over the utterance. Segment `is_final` frames
//! and the open-time Metadata message never end the session — only Metadata
//! after CloseStream (or socket death) does, via the pump's `conclude` helper.
//!
//! The connection is established *concurrently with the first frames*: `open()`
//! returns immediately and audio queues in an unbounded channel until the socket
//! is up. For a multi-second utterance the TLS handshake is entirely hidden.
//!
//! # A short hold can outrun Deepgram entirely
//!
//! For a long utterance the assumption above holds: Deepgram is comfortably
//! caught up by the time the key comes up. For a hold shorter than roughly the
//! connect-plus-first-result latency (~1–3 s, all of it before a single
//! syllable has been transcribed), the key can come up while Deepgram has
//! reported *nothing at all* — `Finalize` then forces out only whatever it has
//! managed to decode so far (often just the first word), and if `CloseStream`
//! follows immediately, the rest of the already-sent audio is simply abandoned
//! mid-processing: the connection that would have carried its transcript is
//! gone. So `CloseStream` is withheld until Deepgram's own reported coverage
//! (`Transcript::covered_secs`, from every message's `start + duration`) has
//! caught up with how much audio the pump actually sent — see `Transcript::
//! caught_up`.
//!
//! That wait ends on *progress*, not on a fixed timer. Coverage often never
//! converges at all — a user who holds the key a second past their last word
//! has sent audio Deepgram will never report words for — so waiting out a
//! fixed ceiling would put that whole ceiling on the perceived latency of an
//! ordinary dictation, and this product's bar for key-release → text is
//! ~300 ms (`docs/spike-findings.md`). Instead the socket is polled on
//! `CATCHUP_STALL`, renewed only by a frame that reports *new* coverage:
//! Deepgram going that long without telling us it got any further *is* the
//! signal that it has nothing more to send, and `CloseStream` goes out
//! immediately. A frame that carries no coverage news — the open-time
//! Metadata, a repeated span — must not renew it, or the wait creeps toward
//! the backstop on traffic that proves nothing. `FINALIZE_TIMEOUT` doubles as an
//! absolute cap on the whole wait, measured from `Finalize`, purely against a
//! socket that chatters without ever converging; reaching it in ordinary use
//! would mean the stall detection is wrong, not that the cap is too tight.
//! Either way the session then signs off normally — closing marginally early
//! costs at most Deepgram's last update, whereas hanging is indistinguishable
//! from a crash.

use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::vlog;

use super::{net, Engine, EngineOptions, Session, TranscriptEvent};

const DEFAULT_MODEL: &str = "nova-3";
const DEFAULT_BASE_URL: &str = "wss://api.deepgram.com/v1/listen";

/// How long to wait for the socket to come up before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long `finish()` may take before we return whatever we have.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
/// Slack allowed between sent and Deepgram-reported-covered audio before
/// `CloseStream` is sent — segment boundaries do not land on exact byte
/// counts, so demanding exact equality would wait forever on a rounding gap.
const CATCHUP_TOLERANCE_SECS: f64 = 0.5;
/// How long Deepgram may go quiet during the catch-up wait before the pump
/// concludes it has nothing further to send and stops withholding
/// `CloseStream`. A live post-`Finalize` flush was measured arriving 356 ms
/// and then 78 ms apart, so this leaves margin over the slowest observed gap
/// while keeping the worst case a fraction of the old fixed ceiling.
const CATCHUP_STALL: Duration = Duration::from_millis(600);

pub struct DeepgramEngine {
    key: String,
    url: String,
}

/// Hand-written so the API key cannot reach a log line, a panic message or a
/// bug report through a stray `{:?}`.
impl std::fmt::Debug for DeepgramEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepgramEngine")
            .field("url", &self.url)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl DeepgramEngine {
    pub fn from_env(opts: &EngineOptions) -> Result<Self> {
        let key = super::require_key(
            "IRIS_DEEPGRAM_KEY",
            "deepgram",
            "https://console.deepgram.com",
        )?;
        let model = opts
            .model
            .clone()
            .or_else(|| std::env::var("IRIS_DEEPGRAM_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base =
            std::env::var("IRIS_DEEPGRAM_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());

        let mut url = format!(
            "{base}?model={model}\
             &encoding=linear16&sample_rate={rate}&channels=1\
             &interim_results=true&punctuate=true&smart_format=true\
             &no_delay=true&endpointing=300",
            rate = crate::audio::SAMPLE_RATE,
        );
        if let Some(lang) = &opts.language {
            url.push_str(&format!("&language={lang}"));
        }

        Ok(Self { key, url })
    }
}

impl Engine for DeepgramEngine {
    fn name(&self) -> &'static str {
        "deepgram"
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        net::init_crypto();
        let rt = net::runtime()?;

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let url = self.url.clone();
        let key = self.key.clone();
        let tx = event_tx.clone();
        rt.spawn(async move {
            if let Err(e) = pump(url, key, audio_rx, tx.clone()).await {
                let _ = tx.send(TranscriptEvent::Error(format!("{e:#}")));
            }
        });

        Ok(Box::new(DeepgramSession {
            audio_tx,
            events: event_rx,
            finished: false,
        }))
    }
}

enum Command {
    Audio(Vec<u8>),
    Finish,
}

struct DeepgramSession {
    audio_tx: UnboundedSender<Command>,
    events: Receiver<TranscriptEvent>,
    finished: bool,
}

impl Session for DeepgramSession {
    fn push(&mut self, pcm: &[i16]) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for &s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        // Unbounded send never blocks. A closed channel means the pump died and
        // already reported the error on `events`, so drop the frame quietly
        // rather than failing the audio path.
        let _ = self.audio_tx.send(Command::Audio(bytes));
        Ok(())
    }

    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }

    fn finish(&mut self) -> Result<()> {
        if !self.finished {
            self.finished = true;
            let _ = self.audio_tx.send(Command::Finish);
        }
        Ok(())
    }
}

/// The post-`Finalize` catch-up wait: how long `CloseStream` keeps being
/// withheld while Deepgram works through audio it has not reported on yet.
/// See the module doc for why this is progress-based rather than a fixed
/// ceiling.
#[derive(Clone, Copy)]
struct CatchupWait {
    /// Renewed by [`CatchupWait::progressed`] on every inbound message.
    stall_at: tokio::time::Instant,
    /// Absolute cap on the whole wait, from when `Finalize` went out. Only a
    /// socket that talks without ever converging should reach it.
    backstop_at: tokio::time::Instant,
}

impl CatchupWait {
    fn start() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            stall_at: now + CATCHUP_STALL,
            backstop_at: now + FINALIZE_TIMEOUT,
        }
    }

    /// The instant to poll the socket until: whichever limit comes first.
    fn deadline(self) -> tokio::time::Instant {
        self.stall_at.min(self.backstop_at)
    }

    fn expired(self) -> bool {
        self.deadline() <= tokio::time::Instant::now()
    }

    /// Deepgram reported covering more audio than before, so it is still
    /// working through the backlog: grant another stall interval. Only a real
    /// advance in coverage counts — a frame that says nothing new about how
    /// far Deepgram has got carries no evidence that it is still going, and
    /// letting it renew the wait is what would push an ordinary dictation onto
    /// the backstop. The backstop itself is deliberately untouched.
    fn progressed(self) -> Self {
        Self {
            stall_at: tokio::time::Instant::now() + CATCHUP_STALL,
            ..self
        }
    }
}

/// Drives one websocket session to completion.
async fn pump(
    url: String,
    key: String,
    mut audio: UnboundedReceiver<Command>,
    events: Sender<TranscriptEvent>,
) -> Result<()> {
    let mut request = url
        .as_str()
        .into_client_request()
        .context("bad Deepgram URL")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {key}")
            .parse()
            .context("bad API key header")?,
    );

    let (mut socket, response) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .context("timed out connecting to Deepgram")?
            .context("connecting to Deepgram (check IRIS_DEEPGRAM_KEY)")?;

    vlog!("deepgram connected: HTTP {}", response.status());
    let _ = events.send(TranscriptEvent::Connected);

    let mut acc = Transcript::default();
    let mut closing = false;
    // Seconds of audio actually sent so far, compared against
    // `acc.covered_secs` to decide when it is safe to send `CloseStream`.
    let mut sent_secs: f64 = 0.0;
    // Set once Finalize is sent while Deepgram has not yet reported covering
    // all the audio that was sent: `CloseStream` is withheld until it does or
    // until Deepgram stops making progress, rather than following Finalize
    // immediately. See the module doc.
    let mut catchup: Option<CatchupWait> = None;
    // Whether `CloseStream` has actually gone out. Distinct from `closing`,
    // which only says Finalize was sent: between the two, Deepgram's Metadata
    // frame cannot be the sign-off, because it has not been asked to sign off.
    let mut closed_stream = false;
    // A socket failure mid-session ends the loop rather than the function:
    // the segments already finalised are the user's words, and `conclude`
    // still gets to return them.
    let mut failure: Option<anyhow::Error> = None;

    loop {
        // While the catch-up wait is pending the socket is polled on *its*
        // deadline: a Deepgram that just goes quiet must not hold the whole
        // dictation for the full finalise timeout.
        let read_timeout = match catchup {
            Some(wait) => wait
                .deadline()
                .saturating_duration_since(tokio::time::Instant::now()),
            None => FINALIZE_TIMEOUT,
        };

        tokio::select! {
            // Bias towards draining audio: an unsent frame is latency we can
            // never get back, whereas a response can wait a few microseconds.
            biased;

            cmd = audio.recv(), if !closing => {
                let sent = match cmd {
                    Some(Command::Audio(bytes)) => {
                        sent_secs += crate::audio::secs(bytes.len() / 2);
                        socket.send(Message::Binary(bytes.into())).await
                            .context("sending audio to Deepgram")
                    }
                    // `finish()` or a dropped session: flush then close.
                    // Finalize asks Deepgram to emit a final for whatever it
                    // has decoded so far; CloseStream then signs off with
                    // Metadata. If Deepgram is already caught up there is
                    // nothing left to wait for, so close right away —
                    // otherwise hold CloseStream until the message loop below
                    // sees `acc.covered_secs` catch up with `sent_secs`.
                    Some(Command::Finish) | None => {
                        closing = true;
                        let fin = socket
                            .send(Message::Text(r#"{"type":"Finalize"}"#.into()))
                            .await
                            .context("finalising the Deepgram stream");
                        match fin {
                            Err(e) => Err(e),
                            Ok(()) if acc.caught_up(sent_secs) => {
                                closed_stream = true;
                                socket
                                    .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
                                    .await
                                    .context("closing the Deepgram stream")
                            }
                            Ok(()) => {
                                catchup = Some(CatchupWait::start());
                                Ok(())
                            }
                        }
                    }
                };
                if let Err(e) = sent {
                    failure = Some(e);
                    break;
                }
            },

            msg = tokio::time::timeout(read_timeout, socket.next()) => {
                // Snapshotted before this iteration can send anything: a frame
                // that *triggers* the close is not a response to it, and must
                // never be read back as its own acknowledgement.
                let was_closed = closed_stream;
                let msg = match msg {
                    Ok(m) => m,
                    // Deepgram has stopped making progress on the backlog (or
                    // the absolute cap ran out): stop withholding CloseStream
                    // and sign off on what we have rather than sitting here.
                    Err(_) if catchup.is_some() => {
                        vlog!("deepgram stopped reporting progress; closing on what it sent");
                        catchup = None;
                        closed_stream = true;
                        let closed = socket
                            .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
                            .await
                            .context("closing the Deepgram stream");
                        if let Err(e) = closed {
                            failure = Some(e);
                            break;
                        }
                        continue;
                    }
                    Err(_) if closing => {
                        vlog!("deepgram finalisation timed out; returning partial result");
                        break;
                    }
                    Err(_) => continue,
                };
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let covered_before = acc.covered_secs;
                        if let Some(update) = acc.absorb(&text) {
                            let _ = events.send(TranscriptEvent::Partial(update));
                        }
                        // Deepgram has reported processing everything that was
                        // sent — or has run out of time to. Either way, ask it
                        // to close. Short of that, only a frame that moved
                        // coverage forward buys another stall interval; one
                        // that did not (a Metadata greeting, a repeated span)
                        // leaves the running stall to expire on schedule.
                        if let Some(wait) = catchup {
                            if acc.caught_up(sent_secs) || wait.expired() {
                                catchup = None;
                                closed_stream = true;
                                let closed = socket
                                    .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
                                    .await
                                    .context("closing the Deepgram stream");
                                if let Err(e) = closed {
                                    failure = Some(e);
                                    break;
                                }
                            } else if acc.covered_secs > covered_before {
                                catchup = Some(wait.progressed());
                            }
                        }
                        // Deepgram may send a Metadata frame at open *and* as
                        // the post-CloseStream sign-off. Only the latter ends
                        // the session; segment `is_final` never does, and
                        // neither does one that arrives before CloseStream had
                        // gone out — a very short hold can drain its whole
                        // audio backlog plus Finish before the socket is
                        // polled once, so the *open* Metadata can land after
                        // Finalize.
                        if acc.done {
                            if was_closed {
                                break;
                            }
                            // Spurious open Metadata: keep the socket open.
                            acc.done = false;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        failure = Some(anyhow::Error::new(e).context("Deepgram socket"));
                        break;
                    }
                }
            }
        }
    }

    if let Some(e) = &failure {
        vlog!("deepgram session failed; salvaging the transcript: {e:#}");
    }
    let _ = events.send(conclude(&acc, sent_secs, failure.map(|e| format!("{e:#}"))));
    Ok(())
}

/// Below this, treat a session as having sent no audio worth transcribing —
/// a stray fraction of a frame is not a meaningfully different situation from
/// none at all. See [`conclude`].
const NEGLIGIBLE_AUDIO_SECS: f64 = 0.1;

/// The single terminal event for a session, however it ended: the transcript
/// when there is one — even after a socket failure, matching what the
/// finalise-timeout path already does — and an error only when there is
/// genuinely nothing to return.
///
/// The two ways to reach an empty transcript are not the same failure and
/// must not share a message: `sent_secs` (tracked in [`pump`] from what was
/// actually forwarded to the socket, independent of anything Deepgram sends
/// back) says which one happened. Leading with the microphone when the real
/// cause is that nothing was captured yet sends the user chasing a hardware
/// problem that does not exist — but zero audio genuinely *can* mean a muted
/// or absent microphone, so the message names the likely cause first and the
/// hardware one second rather than ruling either out.
fn conclude(acc: &Transcript, sent_secs: f64, failure: Option<String>) -> TranscriptEvent {
    let text = acc.finished_text();
    match (text.is_empty(), failure) {
        (false, _) => TranscriptEvent::Final(text),
        (true, Some(err)) => TranscriptEvent::Error(err),
        (true, None) if sent_secs < NEGLIGIBLE_AUDIO_SECS => TranscriptEvent::Error(
            "no audio reached the transcription engine — the key was probably released before \
             recording began, so try holding it a little longer. If it keeps happening, check \
             that the microphone is not muted or unplugged and that Iris is set to the right one"
                .into(),
        ),
        (true, None) => TranscriptEvent::Error(
            "the transcription engine heard the audio but returned no words (likely silence)"
                .into(),
        ),
    }
}

/// Accumulates Deepgram's segmented results into one transcript.
///
/// Deepgram sends interim hypotheses for the current segment and then a final
/// version of it; finalised segments never change again, so the running
/// transcript is `finals + current interim`.
#[derive(Default)]
struct Transcript {
    finals: Vec<String>,
    interim: String,
    done: bool,
    /// The highest `start + duration` reported by any Results message so far
    /// — how much of the sent audio Deepgram has told us it has processed,
    /// regardless of whether that message carried any text. See
    /// [`Transcript::caught_up`].
    covered_secs: f64,
}

impl Transcript {
    /// Whether Deepgram has reported processing at least `sent_secs` of
    /// audio, within [`CATCHUP_TOLERANCE_SECS`]. `CloseStream` must wait for
    /// this after a short hold — see the module doc — or whatever Deepgram
    /// had not gotten to yet is abandoned when the socket closes.
    fn caught_up(&self, sent_secs: f64) -> bool {
        self.covered_secs + CATCHUP_TOLERANCE_SECS >= sent_secs
    }

    /// Feed one JSON message. Returns the updated running transcript when the
    /// message changed it.
    fn absorb(&mut self, json: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;

        match value.get("type").and_then(|t| t.as_str()) {
            Some("Metadata") => {
                // Deepgram sends Metadata at open and again after CloseStream.
                // `done` is sticky here; the pump clears it when `closing` is
                // still false so only the post-CloseStream frame ends the loop.
                self.done = true;
                return None;
            }
            Some("Results") | None => {}
            _ => return None,
        }

        // Every Results message — even an empty one — reports how far into
        // the audio it reaches, which is the signal `caught_up` needs. Update
        // this unconditionally, before the early-returns below, so a message
        // that changes no text still counts as progress.
        let start = value.get("start").and_then(serde_json::Value::as_f64);
        let duration = value.get("duration").and_then(serde_json::Value::as_f64);
        if let (Some(start), Some(duration)) = (start, duration) {
            self.covered_secs = self.covered_secs.max(start + duration);
        }

        let text = value
            .pointer("/channel/alternatives/0/transcript")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let is_final = value
            .get("is_final")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_final {
            // Every finalised segment is kept, including one whose text
            // repeats the segment before it. Deepgram does sometimes re-emit
            // a segment it already finalised (observed live as
            // "Extraordinary." arriving twice for a one-word hold), but text
            // alone cannot tell that apart from a user genuinely saying
            // "No. No." — which endpointing=300 splits into two identical
            // finals, and which arrives entirely after `Finalize` on exactly
            // the short hold this module exists to fix. A duplicated word is
            // strictly better than a deleted one, so the duplicate stays
            // until there is a discriminator that cannot delete real speech.
            if !text.is_empty() {
                self.finals.push(text);
            }
            self.interim.clear();
        } else {
            if text.is_empty() || text == self.interim {
                return None;
            }
            self.interim = text;
        }
        Some(self.running_text())
    }

    fn running_text(&self) -> String {
        let mut parts: Vec<&str> = self.finals.iter().map(String::as_str).collect();
        if !self.interim.is_empty() {
            parts.push(&self.interim);
        }
        parts.join(" ")
    }

    /// The transcript to report as final: interim text is included because a
    /// timed-out flush is better served by a slightly unpolished tail than by
    /// dropping the user's last few words.
    fn finished_text(&self) -> String {
        self.running_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(text: &str, is_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "channel": { "alternatives": [ { "transcript": text } ] }
        })
        .to_string()
    }

    #[test]
    fn interim_results_replace_each_other() {
        let mut t = Transcript::default();
        assert_eq!(t.absorb(&results("the quick", false)).unwrap(), "the quick");
        assert_eq!(
            t.absorb(&results("the quick brown", false)).unwrap(),
            "the quick brown"
        );
        assert_eq!(t.finished_text(), "the quick brown");
    }

    #[test]
    fn finalised_segments_accumulate() {
        let mut t = Transcript::default();
        t.absorb(&results("the quick brown fox.", true));
        t.absorb(&results("iris turns", false));
        assert_eq!(t.finished_text(), "the quick brown fox. iris turns");
        t.absorb(&results("iris turns speech into text.", true));
        assert_eq!(
            t.finished_text(),
            "the quick brown fox. iris turns speech into text."
        );
    }

    #[test]
    fn every_finalised_segment_is_kept_even_when_its_text_repeats() {
        // "No. No." — endpointing=300 splits a genuinely repeated word into
        // two finals with identical text, and on a hold shorter than
        // Deepgram's first response *both* arrive after `Finalize`. Nothing
        // in the text distinguishes that from Deepgram re-emitting a segment,
        // so nothing may be dropped on text alone: a duplicated word is
        // recoverable, a deleted one is gone.
        let mut t = Transcript::default();
        t.absorb(&results("No.", true));
        t.absorb(&results("No.", true));
        assert_eq!(t.finished_text(), "No. No.");

        // Including across an intervening interim, which leaves the previous
        // final in place.
        let mut t = Transcript::default();
        t.absorb(&results("Okay.", true));
        t.absorb(&results("oh", false));
        t.absorb(&results("Okay.", true));
        assert_eq!(t.finished_text(), "Okay. Okay.");
    }

    #[test]
    fn empty_and_duplicate_interims_produce_no_update() {
        let mut t = Transcript::default();
        assert!(t.absorb(&results("", false)).is_none());
        assert!(t.absorb(&results("hello", false)).is_some());
        assert!(t.absorb(&results("hello", false)).is_none());
    }

    #[test]
    fn metadata_ends_the_session() {
        let mut t = Transcript::default();
        t.absorb(&results("done.", true));
        assert!(!t.done);
        t.absorb(r#"{"type":"Metadata","duration":5.4}"#);
        assert!(t.done);
        assert_eq!(t.finished_text(), "done.");
    }

    #[test]
    fn segment_finals_never_mark_the_session_done() {
        // endpointing=300 produces many is_final segments mid-hold; none of
        // them may conclude the session — only Metadata after CloseStream.
        let mut t = Transcript::default();
        t.absorb(&results("Hello.", true));
        assert!(!t.done);
        t.absorb(&results("Why are you not", false));
        t.absorb(&results(
            "Why are you not taking the first words only.",
            true,
        ));
        assert!(!t.done);
        assert_eq!(
            t.finished_text(),
            "Hello. Why are you not taking the first words only."
        );
        t.absorb(r#"{"type":"Metadata","duration":8.9}"#);
        assert!(t.done);
    }

    #[test]
    fn multi_segment_json_fixture_accumulates_like_a_long_hold() {
        // Fixture shaped like a real Deepgram stream: interims, segment final,
        // more interims, segment final, then CloseStream Metadata.
        let msgs = [
            results("hello", false),
            results("hello there", false),
            results("Hello there.", true),
            results("this is the rest", false),
            results("this is the rest of the utterance", false),
            results("This is the rest of the utterance.", true),
            r#"{"type":"Metadata","duration":12.0,"channels":1}"#.to_string(),
        ];
        let mut t = Transcript::default();
        let mut last = String::new();
        for m in &msgs {
            if let Some(update) = t.absorb(m) {
                last = update;
            }
        }
        assert!(t.done, "Metadata must end the accumulator");
        assert_eq!(
            t.finished_text(),
            "Hello there. This is the rest of the utterance."
        );
        // Last Partial update was the second segment final (Metadata yields None).
        assert_eq!(last, "Hello there. This is the rest of the utterance.");
    }

    #[test]
    fn malformed_messages_are_ignored() {
        let mut t = Transcript::default();
        assert!(t.absorb("not json").is_none());
        assert!(t.absorb(r#"{"type":"SpeechStarted"}"#).is_none());
        assert!(!t.done);
    }

    #[test]
    fn a_socket_failure_still_returns_the_accumulated_transcript() {
        let mut t = Transcript::default();
        t.absorb(&results("the quick brown fox.", true));
        t.absorb(&results("and then", false));
        assert_eq!(
            conclude(&t, 3.0, Some("Deepgram socket: connection reset".into())),
            TranscriptEvent::Final("the quick brown fox. and then".into())
        );
    }

    #[test]
    fn a_socket_failure_with_nothing_transcribed_is_an_error() {
        match conclude(
            &Transcript::default(),
            1.0,
            Some("Deepgram socket: connection reset".into()),
        ) {
            TranscriptEvent::Error(msg) => assert!(msg.contains("connection reset")),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_session_concludes_on_content_alone() {
        let mut t = Transcript::default();
        t.absorb(&results("hello.", true));
        assert_eq!(
            conclude(&t, 2.0, None),
            TranscriptEvent::Final("hello.".into())
        );
    }

    #[test]
    fn no_audio_sent_leads_with_the_hold_without_ruling_out_the_microphone() {
        // Symptom B: a hold so short nothing was ever fed to the engine. The
        // original message ("silence, or audio was not reaching the mic") led
        // with a hardware problem that usually did not exist — but zero audio
        // *can* also mean a muted or absent microphone, so naming only the
        // hold would strand a user whose device really is dead.
        match conclude(&Transcript::default(), 0.0, None) {
            TranscriptEvent::Error(msg) => {
                let hold = msg.find("released").expect("must name the short hold");
                let mic = msg.find("microphone").expect("must still name the mic");
                assert!(hold < mic, "the likely cause must come first: {msg}");
                assert!(msg.contains("longer"), "{msg}");
                assert!(msg.contains("muted"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn audio_sent_but_nothing_transcribed_reads_as_silence_not_a_hold_problem() {
        // Real audio reached Deepgram (unlike the case above) and it just had
        // nothing to say about it — a genuinely different situation that must
        // not share a message with the zero-audio case.
        match conclude(&Transcript::default(), 2.5, None) {
            TranscriptEvent::Error(msg) => {
                assert!(msg.contains("silence"), "{msg}");
                assert!(!msg.contains("released"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn from_env_fails_clearly_without_a_key() {
        // Guard against a developer's real key leaking into the test.
        if std::env::var("IRIS_DEEPGRAM_KEY").is_ok() {
            return;
        }
        let err = DeepgramEngine::from_env(&EngineOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("IRIS_DEEPGRAM_KEY"), "unhelpful error: {err}");
    }

    fn results_with_span(text: &str, is_final: bool, start: f64, duration: f64) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "start": start,
            "duration": duration,
            "channel": { "alternatives": [ { "transcript": text } ] }
        })
        .to_string()
    }

    #[test]
    fn caught_up_requires_covering_what_was_sent() {
        let mut t = Transcript::default();
        assert!(!t.caught_up(3.0), "nothing covered yet");

        t.absorb(&results_with_span("Extraordinary.", true, 0.0, 1.0));
        assert!(
            !t.caught_up(3.0),
            "only the first second of three has been covered"
        );

        t.absorb(&results_with_span(
            "Extraordinary. Weather today.",
            true,
            0.0,
            3.0,
        ));
        assert!(t.caught_up(3.0), "now fully covered");
    }

    #[test]
    fn caught_up_allows_small_rounding_slack() {
        let mut t = Transcript::default();
        t.absorb(&results_with_span("hi", true, 0.0, 2.6));
        assert!(
            t.caught_up(2.9),
            "a 0.3s gap is inside CATCHUP_TOLERANCE_SECS"
        );
        assert!(!t.caught_up(4.0), "a 1.4s gap is not");
    }

    #[test]
    fn covered_secs_advances_even_on_an_empty_result() {
        // The trailing empty Results Deepgram sends after the last real
        // segment still reports a `start`/`duration` and must still count as
        // progress, or a real session would never be seen as caught up.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("", true, 3.0, 0.0));
        assert!(t.caught_up(3.0));
    }

    /// A minimal, adversarial fake Deepgram: it reports covering only the
    /// first third of the sent audio when Finalize arrives (exactly run 2's
    /// shape — nothing streamed back during the hold, so the whole utterance
    /// depends on this one forced flush), then only hands over the rest of
    /// the transcript if the client did *not* rush to close the stream. A
    /// client that sends `CloseStream` immediately after `Finalize` (the
    /// pre-fix behaviour) gets an early Metadata sign-off and permanently
    /// loses the remainder — reproducing the real bug on a real, if fake,
    /// wire rather than a hand-written two-message fixture.
    #[tokio::test]
    async fn a_hold_shorter_than_first_response_does_not_abandon_the_backlog() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) if t.contains("Finalize") => break,
                    Some(Ok(_)) => continue,
                    _ => return,
                }
            }

            ws.send(Message::Text(
                results_with_span("Extraordinary.", true, 0.0, 1.0).into(),
            ))
            .await
            .unwrap();

            // Did the client rush to close instead of waiting for Deepgram to
            // finish the rest of the backlog?
            let rushed = tokio::time::timeout(Duration::from_millis(80), ws.next())
                .await
                .is_ok();
            if rushed {
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
                return;
            }

            // A new, additional segment covering the remaining audio (1s-3s)
            // — Deepgram accumulates `is_final` segments, it does not replace
            // them, so this must be the *new* words, not the full sentence.
            ws.send(Message::Text(
                results_with_span("Weather today.", true, 1.0, 2.0).into(),
            ))
            .await
            .unwrap();

            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) if t.contains("CloseStream") => break,
                    Some(Ok(_)) => continue,
                    _ => return,
                }
            }
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"Metadata","duration":3.0}"#.into(),
                ))
                .await;
        });

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // ~3s of "audio" — content is irrelevant off a real mic; only the
        // byte count (what `sent_secs` tracks) matters to the fix under test.
        let three_seconds = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2 * 3];
        audio_tx.send(Command::Audio(three_seconds)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        pump(
            format!("ws://{addr}"),
            "test-key".into(),
            audio_rx,
            event_tx,
        )
        .await
        .unwrap();
        server.await.unwrap();

        let final_text = event_rx.try_iter().find_map(|e| match e {
            TranscriptEvent::Final(t) => Some(t),
            _ => None,
        });
        assert_eq!(
            final_text.as_deref(),
            Some("Extraordinary. Weather today."),
            "the segment Deepgram had not yet reached when Finalize was sent must not be \
             abandoned by a CloseStream that did not wait for it"
        );
    }

    /// Drives `pump` against a scripted fake server and returns whatever it
    /// concluded with, so a test only has to describe the server's behaviour.
    async fn run_pump<F, Fut>(script: F, audio_secs: usize) -> Option<TranscriptEvent>
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            script(ws).await;
        });

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let pcm = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2 * audio_secs];
        audio_tx.send(Command::Audio(pcm)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        pump(
            format!("ws://{addr}"),
            "test-key".into(),
            audio_rx,
            event_tx,
        )
        .await
        .unwrap();
        server.await.unwrap();

        event_rx
            .try_iter()
            .find(|e| matches!(e, TranscriptEvent::Final(_) | TranscriptEvent::Error(_)))
    }

    /// Reads until a message containing `needle` arrives. `false` if the
    /// socket ended first.
    async fn wait_for_text(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        needle: &str,
    ) -> bool {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) if t.contains(needle) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    }

    #[tokio::test]
    async fn the_open_metadata_arriving_after_finalize_does_not_end_the_session() {
        // A hold that fits inside the connect window drains its whole audio
        // backlog *and* Finish before the socket is polled once (the `biased`
        // select), so the Metadata Deepgram sends at open can be read after
        // Finalize. Treating "Finalize was sent" as "CloseStream was sent"
        // made that frame look like the sign-off and abandoned the backlog —
        // exactly the truncation this module exists to prevent.
        let outcome = run_pump(
            |mut ws| async move {
                if !wait_for_text(&mut ws, "Finalize").await {
                    return;
                }
                // The open-time greeting, arriving late.
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":0.0}"#.into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(
                    results_with_span("The whole utterance.", true, 0.0, 3.0).into(),
                ))
                .await
                .unwrap();
                if !wait_for_text(&mut ws, "CloseStream").await {
                    return;
                }
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
            },
            3,
        )
        .await;

        assert_eq!(
            outcome,
            Some(TranscriptEvent::Final("The whole utterance.".into())),
            "a Metadata frame received before CloseStream went out is the open-time greeting, \
             not the sign-off, and must not end the session"
        );
    }

    #[tokio::test]
    async fn a_deepgram_that_goes_quiet_closes_as_soon_as_progress_stalls() {
        // Trailing silence: the user holds the key ~1 s past their last word,
        // so Deepgram's reported coverage never reaches `sent_secs` and it
        // simply goes quiet with nothing more to say. Waiting out a fixed
        // ceiling would put that whole ceiling on an ordinary dictation's
        // perceived latency, so the stall itself is the stop signal — and the
        // words already transcribed must survive it.
        let started = std::time::Instant::now();
        let outcome = run_pump(
            |mut ws| async move {
                if !wait_for_text(&mut ws, "Finalize").await {
                    return;
                }
                // Covers only the first second of the three that were sent,
                // then nothing further ever arrives.
                ws.send(Message::Text(
                    results_with_span("Extraordinary.", true, 0.0, 1.0).into(),
                ))
                .await
                .unwrap();
                if !wait_for_text(&mut ws, "CloseStream").await {
                    return;
                }
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
            },
            3,
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome,
            Some(TranscriptEvent::Final("Extraordinary.".into())),
            "giving up on catch-up must still return what was transcribed"
        );
        assert!(
            elapsed < CATCHUP_STALL * 3,
            "the wait must end on the stall, not on the absolute backstop; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_late_greeting_during_the_catch_up_wait_does_not_end_the_session() {
        // The open-time Metadata arriving mid-wait must be treated as
        // spurious, and the stall stop that follows must still leave the
        // socket open long enough for Deepgram's flush to land.
        let outcome = run_pump(
            |mut ws| async move {
                if !wait_for_text(&mut ws, "Finalize").await {
                    return;
                }
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":0.0}"#.into(),
                ))
                .await
                .unwrap();
                // Nothing else until the client gives up waiting for coverage.
                if !wait_for_text(&mut ws, "CloseStream").await {
                    return;
                }
                ws.send(Message::Text(
                    results_with_span("Everything I said.", true, 0.0, 3.0).into(),
                ))
                .await
                .unwrap();
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
            },
            3,
        )
        .await;

        assert_eq!(
            outcome,
            Some(TranscriptEvent::Final("Everything I said.".into())),
            "a greeting read during the catch-up wait is not the sign-off"
        );
    }

    #[tokio::test]
    async fn a_socket_that_creeps_forward_without_converging_hits_the_backstop() {
        // Coverage that keeps genuinely advancing keeps renewing the stall, so
        // a server inching forward without ever reaching `sent_secs` would
        // hold the wait open indefinitely; `FINALIZE_TIMEOUT`, measured from
        // `Finalize`, caps the whole thing, and the flush that follows
        // CloseStream still lands.
        let started = std::time::Instant::now();
        let outcome = run_pump(
            |mut ws| async move {
                if !wait_for_text(&mut ws, "Finalize").await {
                    return;
                }
                ws.send(Message::Text(
                    results_with_span("Extraordinary.", true, 0.0, 1.0).into(),
                ))
                .await
                .unwrap();
                // Real but minute progress, spaced well inside CATCHUP_STALL
                // so the stall keeps being renewed, and creeping far too
                // slowly to ever reach the three seconds that were sent.
                let deadline = tokio::time::Instant::now() + FINALIZE_TIMEOUT;
                let mut covered = 1.0;
                while tokio::time::Instant::now() < deadline {
                    covered += 0.01;
                    if ws
                        .send(Message::Text(
                            results_with_span("", false, 0.0, covered).into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(CATCHUP_STALL / 3).await;
                }
                if !wait_for_text(&mut ws, "CloseStream").await {
                    return;
                }
                ws.send(Message::Text(
                    results_with_span("Weather today.", true, 1.0, 2.0).into(),
                ))
                .await
                .unwrap();
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
            },
            3,
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome,
            Some(TranscriptEvent::Final(
                "Extraordinary. Weather today.".into()
            )),
            "hitting the backstop must close normally, not discard the flush"
        );
        assert!(
            elapsed >= FINALIZE_TIMEOUT,
            "the backstop, not the stall, is what should have ended this wait; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_frame_carrying_no_new_coverage_does_not_extend_the_wait() {
        // The stall runs from the last frame that actually reported progress.
        // A Metadata greeting says nothing about how far Deepgram has got, so
        // it must not buy another interval — otherwise every non-advancing
        // frame walks an ordinary dictation toward the 5 s backstop.
        let closed_after = std::sync::Arc::new(std::sync::Mutex::new(Duration::ZERO));
        let recorded = std::sync::Arc::clone(&closed_after);
        let outcome = run_pump(
            move |mut ws| async move {
                if !wait_for_text(&mut ws, "Finalize").await {
                    return;
                }
                ws.send(Message::Text(
                    results_with_span("Extraordinary.", true, 0.0, 1.0).into(),
                ))
                .await
                .unwrap();
                let advanced_at = tokio::time::Instant::now();

                // Two thirds of the way through the stall window, so renewing
                // on this frame would push the close out to 1⅔ intervals.
                tokio::time::sleep(CATCHUP_STALL * 2 / 3).await;
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":0.0}"#.into(),
                ))
                .await
                .unwrap();

                if !wait_for_text(&mut ws, "CloseStream").await {
                    return;
                }
                *recorded.lock().expect("timing") = advanced_at.elapsed();
                ws.send(Message::Text(
                    results_with_span("Weather today.", true, 1.0, 2.0).into(),
                ))
                .await
                .unwrap();
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","duration":3.0}"#.into(),
                    ))
                    .await;
            },
            3,
        )
        .await;

        let waited = *closed_after.lock().expect("timing");
        assert_eq!(
            outcome,
            Some(TranscriptEvent::Final(
                "Extraordinary. Weather today.".into()
            )),
            "closing on the stall must still collect the flush"
        );
        assert!(
            waited < CATCHUP_STALL + CATCHUP_STALL / 3,
            "the stall must run from the last frame that advanced coverage, not from the \
             Metadata that carried none; CloseStream came {waited:?} after it"
        );
    }
}
