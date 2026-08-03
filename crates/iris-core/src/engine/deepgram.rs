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
//! # The warm spare: `WarmPool`
//!
//! Even hidden behind speech, every dictation still pays a fresh connect —
//! live-measured `stream_ready_ms` (`AGENTS.md`) runs 0.9-4s, and a hold
//! shorter than that has the connect latency exposed directly, which is what
//! `from_finalize` (below) exists to survive rather than avoid. Naive
//! prewarming — open one connection ahead of the key press and hope it is
//! still there — was tried and reverted (see `AGENTS.md`, `app.rs`): a live
//! idle probe found Deepgram closes an unused connection in roughly 12-15s,
//! far short of the gaps between real dictations, so a static prewarm was
//! usually dead by the time it mattered.
//!
//! `WarmPool` is a different shape, not a resurrection of that one: it keeps
//! at most one spare connection *actively* alive with Deepgram's documented
//! `{"type":"KeepAlive"}` control frame (sent every
//! [`WARM_KEEPALIVE_INTERVAL`], well under the idle-close window), for up to
//! [`MAX_WARM_IDLE`] since it was last handed to a real dictation. `open()`
//! takes the spare if one is ready and hands it straight to `pump_inner`,
//! skipping the connect entirely; otherwise it connects cold exactly as
//! before. Either way a fresh spare is queued to replace whatever was just
//! taken, so the *next* dictation has one waiting again.
//!
//! Deepgram never acknowledges a `KeepAlive` (its docs are explicit: no
//! response), so a successful send is proof the local write succeeded, not
//! that the peer is still there — a connection can go dead in the gap
//! between the last keep-alive and the moment `open()` claims it, and no
//! amount of pinging closes that window to zero. Rather than accept that as
//! a data-loss risk, `pump_inner` treats a handed-in spare as unconfirmed
//! until its first send succeeds: if that first send fails, it transparently
//! reconnects cold and retries the same bytes before giving up, so a stale
//! handoff costs a bit of latency (falls back to exactly today's cold-connect
//! path) and never a dropped word. See the `confirm_pending` handling in
//! `pump_inner` and `a_stale_warm_handoff_reconnects_without_losing_audio`.
//!
//! # Waiting for Deepgram to actually be done: `from_finalize`
//!
//! A hold shorter than roughly the connect-plus-first-result latency (~1-3s)
//! can end with Deepgram having reported *nothing at all* — sending
//! `Finalize` then `CloseStream` immediately, back to back, gives Deepgram no
//! chance to flush the backlog it has not gotten to yet before the socket
//! that would have carried the transcript is gone.
//!
//! Three designs were tried and rejected before this one: an unbounded wait
//! for reported coverage to catch up with audio sent (could hang
//! indefinitely on trailing silence Deepgram has nothing further to say
//! about); a fixed-ceiling version of the same (added measurable latency to
//! the *ordinary* case of a user pausing before releasing, and — because the
//! ceiling reused the same tolerance as "is the gap close enough" — silently
//! skipped waiting altogether for any hold under ~0.5s, exactly reopening the
//! bug it existed to close); and a stall-detector version (still guessing,
//! just with a shorter guess). Every one of them was inferring "Deepgram is
//! done" from an *absence* of evidence, which is structurally in tension with
//! feeling instant: silence cannot be told apart from "about to respond"
//! without spending time to find out.
//!
//! `from_finalize` sidesteps the guess entirely. It is a boolean Deepgram
//! itself sets on a Results message to mean "this is the flush you asked for
//! with Finalize" — authoritative rather than inferred, live-verified (see
//! `Transcript::finalize_acked`) to arrive reliably whenever any audio was
//! actually sent, including for holds under 0.5s and for holds containing
//! only silence (an empty-but-`from_finalize`-tagged result). It does *not*
//! arrive when literally nothing was sent, so that case is short-circuited on
//! `sent_secs == 0.0` in `pump_inner` — exact, not a small-but-nonzero
//! threshold; see `NEGLIGIBLE_AUDIO_SECS`'s own doc for why those two checks
//! must stay separate. `FINALIZE_ACK_TIMEOUT` is a bounded safety net under
//! this for protocol failure only — a missing or malformed signal — and
//! should never be reached in normal operation.
//!
//! The ack decides *when `CloseStream` goes out*, and nothing else. Reporting
//! `TranscriptEvent::Final` on it too — cutting the `CloseStream` + Metadata
//! sign-off round trip off the user's perceived latency — was built and then
//! rejected, because the ack only proves the flush *started*, not that it fit
//! in one frame, and Deepgram documents no one-tagged-result-per-`Finalize`
//! guarantee. Covering a split flush needs a quiet window after the ack, and
//! a live measurement of this codebase's own steady-state message cadence (a
//! 10.8s multi-sentence WAV with natural pauses, timestamped `--verbose`
//! output) found gaps of 24.6ms, 152.6ms and 246.0ms between exactly the
//! segment-boundary message pairs a split flush would look like. A window
//! that actually covers 246ms puts finalisation at roughly 200ms (the ack
//! floor) + 246ms, and a window with real margin over it costs more still —
//! against a ~280ms budget in `docs/spike-findings.md`. No interval both
//! covers what was measured and clears that budget, so `Final` is emitted
//! exactly once at the bottom of `pump_inner` after the session actually
//! closes, and the sign-off round trip is accepted rather than traded for a
//! transcript that can be short.
//!
//! Gating on the session close is what makes sending `CloseStream` on the
//! first ack safe, and the reason is structural rather than a won race. Three
//! things hold in `pump_inner`: `acc.absorb` runs on every inbound Text frame
//! unconditionally, with no guard that could suppress it once `CloseStream`
//! has gone out; the loop breaks only on the Metadata sign-off that follows
//! `CloseStream`, a socket Close/None, or a failure, and every one of those
//! paths falls through to the single `conclude(&acc, ..)` at the bottom; and
//! one websocket over one TCP connection delivers frames in exactly the order
//! the server sent them, so no reordering is possible. Together those mean
//! anything Deepgram actually transmits before its own sign-off is absorbed
//! and reported — a continuation landing between `CloseStream` and the
//! Metadata is still the user's text.
//!
//! What that reasoning does *not* cover is Deepgram sending its sign-off
//! before it finished transmitting a multi-part flush it had already started
//! — the server abandoning its own in-progress response on `CloseStream`
//! instead of completing it. That is a question about server behaviour, not a
//! client-side timing race, and it would contradict the meaning of
//! Metadata-after-`CloseStream` ("I am done responding") that this whole
//! mechanism already rests on. It has never been observed here: the
//! 24.6/152.6/246.0ms cadence above is evidence about ordinary streaming, not
//! about flush abandonment. Closing it would need the quiet window that was
//! measured and rejected on latency grounds, so it stands as a known residual
//! risk.
//!
//! # Suppressing a re-emitted segment: time-span containment
//!
//! Deepgram occasionally re-emits a segment it already sent an `is_final`
//! result for — the same words, tagged final a second time. A first attempt
//! at a guard for this (a previous branch, since removed) narrowed detection
//! to the window right after a `Finalize` ack, on the theory that anything
//! landing there is provably a repeat rather than new speech. That premise is
//! false: a hold shorter than connect-plus-first-result (see above) has
//! *everything* arrive after `Finalize`, including a genuine "No. No." A
//! guard keyed on *when* a result arrives cannot tell an artefact from real
//! speech in exactly the case it most needs to, so it was removed rather than
//! shipped.
//!
//! The discriminator that survives is *what audio the result covers*, not
//! when it arrived or what it says. Every streaming `Results` message carries
//! top-level `start` and `duration` fields (seconds into the utterance, and
//! the length covered — confirmed against Deepgram's current streaming
//! reference; distinct from the per-word `start`/`end` timestamps nested
//! under `channel.alternatives[0].words`, which this guard does not need: the
//! existing accumulator already keys accepted text by segment, and a
//! segment-level span is the natural unit to compare against segments already
//! held). A re-emission covers audio that was already accepted; a genuinely
//! repeated word — "No. No." — occupies a later span even though the text is
//! identical. So [`Transcript`] keeps the `(start, start + duration)` span
//! alongside each accepted final segment.
//!
//! The rule on those spans is **containment, not overlap**: a new final
//! segment is withheld only when *every part of it* is already covered by
//! the spans accepted so far (`is_fully_covered`, which walks the accepted
//! spans as one coverage set, so a re-emission that merges two adjacent
//! accepted segments is recognised too). Overlap alone is not enough, and
//! deliberately so. Deepgram can revise a segment boundary or merge
//! segments, producing a final that starts inside accepted audio and then
//! runs on into speech nobody has transcribed yet — `[0.0, 1.5] "the quick
//! brown fox"` followed by `[1.4, 5.0] "fox jumps over the lazy dog"`. An
//! overlap rule drops that whole second segment and with it six words the
//! user actually said. Containment keeps it, in full: the seam may duplicate
//! a word, and that is the cheaper error by the rule this whole feature
//! exists under (a wrongly-deleted word is strictly worse than a duplicated
//! one). What is *not* done is trimming the covered prefix off such a
//! segment before keeping it: cutting words out of a transcript by timing
//! arithmetic is a bigger risk than the duplicate it would avoid, and more
//! machinery than this guard is meant to carry.
//!
//! No comparison in that containment test adds slack anywhere, and that is
//! what enforces the direction rather than any claim about it. The coverage
//! walk rejects a gap outright — `start > covered_to` ends it with nothing
//! forgiven, so uncovered audio stays uncovered however many accepted spans
//! the walk crosses, and no per-span allowance can accumulate — and the
//! decision itself is the plain `covered_to >= new_span.1`. A new segment
//! whose end exceeds the coverage by any amount at all, however small, is
//! kept. That exact comparison still catches what it must: a re-emission
//! whose end lands exactly on the coverage boundary, such as one merging two
//! exactly-adjacent accepted spans, is covered and suppressed. Widening
//! either side would be the wrong direction — comparing against
//! `covered_to + tolerance` would let a boundary that merely comes *close*
//! to being covered count as covered, which is how a guard like this deletes
//! speech, and demanding `covered_to >= new_span.1 + tolerance` would let a
//! plain exact-match duplicate through.
//!
//! [`SPAN_TOLERANCE_SECS`] (1 ms) is therefore consumed in exactly one place:
//! a new span whose own duration is at or under it is never treated as
//! covered at all. Deepgram documents its streaming timestamps as not
//! guaranteed accurate below millisecond precision even though the JSON
//! carries more decimal digits than that, and an accepted segment's end is
//! our own `start + duration` in f64 while the next segment's start is an
//! independently formatted decimal — at that scale the two numbers are noise
//! rather than evidence, so a span that short cannot be classified with
//! confidence and is kept. That use only ever removes suppression cases; it
//! can never create one.
//!
//! Boundary noise between adjoining segments needs no tolerance under this
//! rule, which is why none is applied there. The case that originally seemed
//! to need one — a first "No." reported as `start 0.0, duration 0.4400001`
//! and a second as `start 0.44`, overlapping by a hair — is decided by
//! containment alone: the second span runs 0.44 s past everything accepted,
//! so it is kept. It is the move from overlap to containment that saves that
//! word, not a tolerance.
//!
//! Suppression only ever withholds a *new* segment from being added; it never
//! removes anything already accepted, and it leaves the in-flight interim
//! alone (a re-emission of old audio says nothing about the not-yet-final
//! segment currently accumulating, and `finished_text` reports that interim
//! text). That asymmetry is deliberate. It also means a re-emission carrying
//! a revised transcription of fully-covered audio is not merged in even
//! though it might be an improvement: taking that upside would require
//! replacing already-shown text, which is out of scope for a guard whose only
//! job is not to double it. Because a suppression does remove words from
//! what the user would otherwise see, it is logged under `--verbose` like
//! every other consequential decision in this module.
//!
//! How often any of this fires in production is unknown, and should not be
//! assumed. Suppression needs a re-emission whose span is contained in
//! accepted coverage under the exact comparison above; the tests here assume
//! that shape (a subset span like `[0.2, 1.2]` inside `[0.0, 1.5]`) because
//! that is how they were written, not because it was observed in captured
//! traffic. If real re-emissions instead arrive with slightly wider
//! boundaries than the finals they repeat, this guard never fires and the
//! duplicate still reaches the user. That is the conservative end of the
//! tradeoff this feature accepts, and it is deliberately not widened to
//! catch more — a duplicate is cheaper than a deletion. The `--verbose` log
//! line on the suppressed branch is what would settle it against a real
//! session; until then the containment shape is an assumption, not a
//! measurement.
//!
//! Missing or unusable timing — no `start`/`duration` on the message, a
//! non-finite value, a negative `start`, or a non-positive `duration` (a
//! zero-or-negative span asserts nothing about what it covers) — makes the
//! new segment's span `None`. A `None` span is never covered by anything, by
//! construction (`parse_span` is the only place a `Some` is produced, and
//! `is_fully_covered` is only ever reached through one), so an ambiguous
//! result is always kept. This is the same bias the rest of the module
//! applies to uncertainty: when the evidence needed to act is missing, do
//! nothing rather than guess.
//!
//! The check costs one linear scan over the segments already accepted in
//! this session (bounded by how many finals a single dictation produces —
//! never large) and adds no waiting: it runs synchronously inside `absorb`,
//! on data already in hand, so it cannot add latency to when a result is
//! reported. It does not touch `from_finalize`, `FINALIZE_ACK_TIMEOUT`, or
//! any part of the catch-up mechanism above — this is a separate comparison
//! at the point a segment is accepted, not another layer on top of deciding
//! when to stop waiting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use futures_util::{Sink, SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::vlog;

use super::{net, Engine, EngineOptions, Session, TranscriptEvent};

const DEFAULT_MODEL: &str = "nova-3";
const DEFAULT_BASE_URL: &str = "wss://api.deepgram.com/v1/listen";

/// How long to wait for the socket to come up before giving up.
///
/// Sized for margin over a real connect, not to fit inside anything: the worst
/// successful connect in the captain's session log was 4.85s (during a
/// degraded-network window; every other one under ~4.1s), so this leaves ~3.15s
/// of headroom. Squeezing it under `DEFAULT_FINAL_TIMEOUT` was tried and
/// reverted — the two clocks start at different instants (this one at key-down,
/// that deadline at key-up), so there is no length of hold at which the ordering
/// is stable, and paying for it with connects that used to succeed bought
/// nothing.
///
/// The outer bound is not allowed to win that race and lose the utterance
/// either, so this value is published through [`Engine::connect_budget`] and
/// `Dictation::finish` waits it out when a connect is all that is outstanding,
/// then gives the socket that comes up late its ordinary finalise budget from
/// there — the audio a late connect flushes is exactly what those seconds buy.
/// A connect that fails on its own terms still reports as one — that
/// diagnosability was the point of the earlier arrangement and it survives —
/// but it now reports after the budget it was given, not before.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How often the warm spare gets a `KeepAlive` frame, comfortably under
/// Deepgram's ~10-15s idle-connection close (see `WarmPool`'s doc).
const WARM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);
/// How long the warm spare is worth maintaining after the last dictation.
/// Bounded rather than indefinite: live-measured against the captain's own
/// session log (`AGENTS.md`), 61% of the gaps between real dictations were
/// under 3 minutes, versus 29-37% under Deepgram's own idle-close window
/// alone — most of the benefit, without holding a connection open across the
/// much longer gaps (tens of minutes to hours) that dominate wall-clock time
/// between sessions.
const MAX_WARM_IDLE: Duration = Duration::from_secs(180);
/// How long `finish()` may take, once `CloseStream` has actually been sent,
/// before we return whatever we have. Re-armed on every inbound message, so
/// this bounds *silence*, not the whole finalisation — see
/// `FINALIZE_ACK_TIMEOUT` for the wait before `CloseStream` goes out.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
/// Absolute safety net for the wait on `from_finalize`, covering only
/// protocol failure (a missing or malformed signal) — not a tuning knob.
/// Live-measured `from_finalize` latency across a range of hold shapes
/// (350ms speech, 400ms and 50ms of silence, a 5.6s multi-segment utterance)
/// was consistently 200-550ms; this leaves a wide margin without adding
/// meaningful latency to any real session, and it should never be reached.
const FINALIZE_ACK_TIMEOUT: Duration = Duration::from_secs(3);
/// Below this, [`conclude`]'s empty-transcript error names the hold as the
/// cause rather than silence. Wording only — a user who released this fast
/// should be told they released too early, not that Deepgram heard them and
/// stayed quiet. This must never gate whether `pump_inner` *waits* for
/// `from_finalize`: live evidence shows Deepgram acks a 50 ms hold, well
/// under this, so any audio-vs-silence threshold here would skip the wait
/// for real speech. That decision uses `sent_secs == 0.0` directly (see
/// `pump_inner`) — genuinely zero audio, not a smaller version of this
/// constant. Keeping these as two unrelated checks, not one shared
/// threshold, is deliberate: reusing a single constant for "how should we
/// word this" and "should we wait" is exactly the bug class this module's
/// rejected stall-detector design shipped with.
const NEGLIGIBLE_AUDIO_SECS: f64 = 0.1;

pub struct DeepgramEngine {
    key: String,
    url: String,
    warm: WarmPool,
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

        Ok(Self {
            key,
            url,
            warm: WarmPool::new(),
        })
    }
}

impl Engine for DeepgramEngine {
    fn name(&self) -> &'static str {
        "deepgram"
    }

    /// See [`CONNECT_TIMEOUT`]. Deepgram is the default engine and the one the
    /// 6s outer bound was measured against, so this pairing is the one that
    /// has to hold: without it, a hold shorter than the gap between the two
    /// clocks ends a connect the engine was still entitled to finish.
    fn connect_budget(&self) -> Option<Duration> {
        Some(CONNECT_TIMEOUT)
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        net::init_crypto();
        let rt = net::runtime()?;

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // Non-blocking: a maintenance tick holding the lock for a few
        // microseconds must never make the key-press path wait, so a
        // momentary contention is treated the same as "no spare ready" and
        // falls through to a cold connect exactly as before.
        let warm_socket = self.warm.try_take();
        self.warm
            .ensure_maintaining(self.url.clone(), self.key.clone());

        let url = self.url.clone();
        let key = self.key.clone();
        let tx = event_tx.clone();
        rt.spawn(async move {
            if let Err(e) = pump(url, key, audio_rx, tx.clone(), warm_socket).await {
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

/// A connected-but-not-yet-used websocket, however it was obtained.
type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a fresh, authenticated websocket to Deepgram. The one place a cold
/// connect happens — both [`pump_inner`]'s own fallback and [`WarmPool`]'s
/// maintenance loop call this, so there is exactly one connect
/// implementation to keep correct.
async fn connect(url: &str, key: &str) -> Result<WsStream> {
    let mut request = url.into_client_request().context("bad Deepgram URL")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {key}")
            .parse()
            .context("bad API key header")?,
    );

    // A shared TLS config (see `net::tls_connector`'s doc) lets rustls resume
    // a session on the second and later connect in this process's lifetime
    // instead of paying a full handshake every single dictation.
    let connector = net::tls_connector();
    let (socket, response) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(connector)),
        ),
    )
    .await
    .context("timed out connecting to Deepgram")?
    .context("connecting to Deepgram (check IRIS_DEEPGRAM_KEY)")?;

    vlog!("deepgram connected: HTTP {}", response.status());
    Ok(socket)
}

/// Sends `msg` on `socket`, clearing `confirm_pending` the moment any send
/// succeeds. If this is the first send since a warm handoff
/// (`confirm_pending` still true) and it fails, reconnects cold exactly once
/// and retries the same message before giving up, rather than treating an
/// unproven spare's failure as a session failure — see the module doc's "The
/// warm spare" section. A cold-started session always has `confirm_pending`
/// already false, so this adds nothing on that path beyond the flag check.
async fn send_confirming(
    socket: &mut WsStream,
    confirm_pending: &mut bool,
    url: &str,
    key: &str,
    msg: Message,
    context_msg: &'static str,
) -> Result<()> {
    if !*confirm_pending {
        return socket.send(msg).await.context(context_msg);
    }
    match socket.send(msg.clone()).await {
        Ok(()) => {
            *confirm_pending = false;
            Ok(())
        }
        Err(e) => {
            vlog!("deepgram: warm spare's first send failed ({e}); reconnecting cold");
            *socket = connect(url, key).await?;
            *confirm_pending = false;
            socket.send(msg).await.context(context_msg)
        }
    }
}

/// Keeps at most one spare Deepgram connection alive across the gap between
/// dictations. See the module doc's "The warm spare" section for why this is
/// a different shape from the prewarming that was tried and reverted.
#[derive(Clone)]
struct WarmPool {
    slot: Arc<Mutex<Option<(WsStream, Instant)>>>,
    maintaining: Arc<std::sync::atomic::AtomicBool>,
}

impl WarmPool {
    fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
            maintaining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Take the spare if one is ready right now. Never blocks: a maintenance
    /// tick holding the lock for the microseconds a send takes is treated the
    /// same as "nothing ready" rather than making the key-press path wait.
    fn try_take(&self) -> Option<WsStream> {
        self.slot.try_lock().ok()?.take().map(|(socket, _)| socket)
    }

    /// Start the maintenance loop the first time this is called; every call
    /// after that is a no-op. Called from every `open()` rather than once in
    /// `from_env` so the loop naturally lives on the same Tokio runtime
    /// `open()` already required to exist.
    fn ensure_maintaining(&self, url: String, key: String) {
        if self
            .maintaining
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let slot = Arc::clone(&self.slot);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WARM_KEEPALIVE_INTERVAL).await;
                let mut guard = slot.lock().await;
                match guard.as_mut() {
                    Some((_, established_at)) if established_at.elapsed() > MAX_WARM_IDLE => {
                        vlog!("deepgram: warm spare idle past {MAX_WARM_IDLE:?}; letting it go");
                        *guard = None;
                    }
                    Some((socket, _)) => {
                        // Deepgram never acknowledges this frame (see the
                        // module doc); a send failure is the only signal
                        // available here that the spare has died.
                        if socket
                            .send(Message::Text(r#"{"type":"KeepAlive"}"#.into()))
                            .await
                            .is_err()
                        {
                            vlog!("deepgram: warm spare died; will reconnect");
                            *guard = None;
                        }
                    }
                    None => match connect(&url, &key).await {
                        Ok(socket) => *guard = Some((socket, Instant::now())),
                        // No backoff: the next tick (WARM_KEEPALIVE_INTERVAL
                        // away) retrying is gentle enough on its own, and a
                        // real dictation never waits on this loop — a failed
                        // warm connect just means the next `open()` connects
                        // cold, exactly as if this loop did not exist.
                        Err(e) => vlog!("deepgram: warm spare connect failed: {e:#}"),
                    },
                }
            }
        });
    }
}

/// Drives one websocket session to completion, waiting on `from_finalize`
/// with [`FINALIZE_ACK_TIMEOUT`] as its safety net. A thin wrapper over
/// [`pump_inner`] so tests can shrink that timeout instead of spending it for
/// real on the one path meant to (essentially) never take it.
async fn pump(
    url: String,
    key: String,
    audio: UnboundedReceiver<Command>,
    events: Sender<TranscriptEvent>,
    warm: Option<WsStream>,
) -> Result<()> {
    pump_inner(url, key, audio, events, FINALIZE_ACK_TIMEOUT, warm).await
}

async fn pump_inner(
    url: String,
    key: String,
    mut audio: UnboundedReceiver<Command>,
    events: Sender<TranscriptEvent>,
    finalize_ack_timeout: Duration,
    warm: Option<WsStream>,
) -> Result<()> {
    // A handed-in spare has only ever exchanged KeepAlive frames Deepgram
    // does not acknowledge (see the module doc), so it is not proven live
    // until this session's own first send on it succeeds. `pump_inner`'s
    // audio-send arm below reconnects cold and retries, exactly once, if
    // that first send fails — see `confirm_pending`.
    let mut confirm_pending = warm.is_some();
    let mut socket = match warm {
        Some(socket) => {
            vlog!("deepgram: reusing a warm connection");
            let _ = events.send(TranscriptEvent::Connected);
            socket
        }
        None => {
            let socket = connect(&url, &key).await?;
            let _ = events.send(TranscriptEvent::Connected);
            socket
        }
    };
    let mut acc = Transcript::default();
    let mut closing = false;
    // Seconds of audio actually sent so far — used only to short-circuit the
    // `from_finalize` wait when there was genuinely nothing to flush, and to
    // tell the two empty-transcript error messages apart in `conclude`.
    let mut sent_secs: f64 = 0.0;
    // Whether `CloseStream` has actually been sent, as opposed to `closing`
    // (Finalize sent / no more audio accepted) — a Metadata frame only ends
    // the session once this is true. Keeping the two states distinct is what
    // stops a late open-time Metadata (the whole hold can fit inside the
    // connect window, so the socket may not be polled even once before
    // Finalize goes out) from being misread as the close sign-off before
    // `CloseStream` was ever sent.
    let mut closed_stream = false;
    // Some(deadline) while waiting specifically for `from_finalize` — see the
    // module doc. Absolute, not renewed by intervening messages: it is a
    // safety net for protocol failure, not a progress heuristic.
    let mut finalize_ack_deadline: Option<Instant> = None;
    // A socket failure mid-session ends the loop rather than the function:
    // the segments already finalised are the user's words, and `conclude`
    // still gets to return them.
    let mut failure: Option<anyhow::Error> = None;

    loop {
        let poll_timeout = match finalize_ack_deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
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
                        send_confirming(
                            &mut socket,
                            &mut confirm_pending,
                            &url,
                            &key,
                            Message::Binary(bytes.into()),
                            "sending audio to Deepgram",
                        ).await
                    }
                    // `finish()` or a dropped session: flush then close.
                    // Finalize asks Deepgram to emit a from_finalize-tagged
                    // result for whatever it has buffered; with nothing ever
                    // sent there is nothing for it to tag, so close right
                    // away instead of waiting for a signal that will not come.
                    Some(Command::Finish) | None => {
                        closing = true;
                        let fin = send_confirming(
                            &mut socket,
                            &mut confirm_pending,
                            &url,
                            &key,
                            Message::Text(r#"{"type":"Finalize"}"#.into()),
                            "finalising the Deepgram stream",
                        ).await;
                        match fin {
                            Err(e) => Err(e),
                            Ok(()) => {
                                // Only a tag absorbed strictly after the
                                // Finalize is on the wire can be *this*
                                // Finalize's ack. Anything seen before it
                                // (Deepgram should never send one, but the
                                // whole design rests on this signal being
                                // authoritative) is discarded here rather than
                                // left to satisfy the wait armed just below.
                                acc.finalize_acked = false;
                                // Exactly zero, not a smaller version of
                                // NEGLIGIBLE_AUDIO_SECS: sent_secs only ever
                                // grows from real byte counts, so this is
                                // exact and safe, and it is the only duration
                                // live-verified not to produce a from_finalize
                                // ack. Anything above it — even the 50 ms this
                                // module's tests hold it to — can carry real
                                // speech and must wait.
                                if sent_secs == 0.0 {
                                    send_close_stream(&mut socket, &mut closed_stream).await
                                } else {
                                    finalize_ack_deadline =
                                        Some(Instant::now() + finalize_ack_timeout);
                                    Ok(())
                                }
                            }
                        }
                    }
                };
                if let Err(e) = sent {
                    failure = Some(e);
                    break;
                }
            },

            msg = tokio::time::timeout(poll_timeout, socket.next()) => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) if finalize_ack_deadline.is_some() => {
                        // Safety net only: Deepgram never confirmed the flush
                        // within a generous margin over its measured typical
                        // latency. Accept whatever has been received rather
                        // than hang.
                        vlog!(
                            "no from_finalize within {:?}; closing with what was received",
                            finalize_ack_timeout
                        );
                        finalize_ack_deadline = None;
                        if let Err(e) = send_close_stream(&mut socket, &mut closed_stream).await {
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
                        // Captured before `absorb`: a message that itself
                        // *triggers* closing (below) is not a response to
                        // that closing, and must never be read back as its
                        // own sign-off.
                        let was_closed_stream = closed_stream;
                        if let Some(update) = acc.absorb(&text) {
                            let _ = events.send(TranscriptEvent::Partial(update));
                        }
                        if finalize_ack_deadline.is_some() && acc.finalize_acked {
                            finalize_ack_deadline = None;
                            // Closing here costs nothing the user can lose:
                            // frames arrive in send order, `absorb` above runs
                            // on every one of them regardless of this, and the
                            // single `conclude` at the bottom reports whatever
                            // has accumulated by the sign-off. The residual
                            // risk is the server abandoning its own
                            // in-progress flush before that sign-off — see the
                            // module doc.
                            if let Err(e) = send_close_stream(&mut socket, &mut closed_stream).await {
                                failure = Some(e);
                                break;
                            }
                        }
                        // Deepgram may send a Metadata frame at open *and* as
                        // the post-CloseStream sign-off. Only the latter ends
                        // the session; segment `is_final` never does.
                        if acc.done {
                            if was_closed_stream {
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

/// Send `CloseStream` and record that it went out. The one place this
/// happens, so `closed_stream` can never be set without it actually being
/// sent — see its role in distinguishing a real sign-off from a stray
/// open-time Metadata in [`pump_inner`].
async fn send_close_stream<S>(socket: &mut S, closed_stream: &mut bool) -> Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    socket
        .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
        .await
        .context("closing the Deepgram stream")?;
    *closed_stream = true;
    Ok(())
}

/// The single terminal event for a session, however it ended: the transcript
/// when there is one — even after a socket failure, matching what the
/// finalise-timeout path already does — and an error only when there is
/// genuinely nothing to return.
///
/// The three ways to reach an empty transcript are not the same failure and
/// must not share a message. `sent_secs` (tracked in [`pump_inner`] from what
/// was actually forwarded to the socket, independent of anything Deepgram
/// sends back) separates "nothing was captured yet" from the rest, and
/// [`Transcript::finalize_acked`] separates "Deepgram confirmed the flush and
/// had no words" from "Deepgram never confirmed anything" — the protocol
/// failure [`FINALIZE_ACK_TIMEOUT`] exists to bound. Reporting silence for
/// either of the other two sends the user chasing a microphone or a volume
/// problem that does not exist.
fn conclude(acc: &Transcript, sent_secs: f64, failure: Option<String>) -> TranscriptEvent {
    let text = acc.finished_text();
    match (text.is_empty(), failure) {
        (false, _) => TranscriptEvent::Final(text),
        (true, Some(err)) => TranscriptEvent::Error(err),
        (true, None) if sent_secs < NEGLIGIBLE_AUDIO_SECS => TranscriptEvent::Error(
            "almost no audio reached the transcription engine — the key was likely released \
             before recording could start; try holding it a little longer"
                .into(),
        ),
        (true, None) if !acc.finalize_acked => TranscriptEvent::Error(
            "the transcription engine never confirmed receiving the audio in time, so nothing \
             came back; please try again"
                .into(),
        ),
        (true, None) => TranscriptEvent::Error(
            "the transcription engine heard the audio but returned no words (likely silence)"
                .into(),
        ),
    }
}

/// One accepted final segment: its text, and the audio span it covers when
/// Deepgram reported usable timing for it. See the module doc's "Suppressing
/// a re-emitted segment" section.
struct FinalSegment {
    text: String,
    span: Option<(f64, f64)>,
}

/// Parse the top-level `start`/`duration` timing off a Results message into
/// an (start, end) span, or `None` when the timing is missing or not usable
/// as evidence — the only two outcomes, by design, so a caller can never
/// mistake absent timing for a zero-width or zero-based span.
fn parse_span(value: &serde_json::Value) -> Option<(f64, f64)> {
    let start = value.get("start").and_then(|v| v.as_f64())?;
    let duration = value.get("duration").and_then(|v| v.as_f64())?;
    if !start.is_finite() || !duration.is_finite() || start < 0.0 || duration <= 0.0 {
        return None;
    }
    Some((start, start + duration))
}

/// The span length under which Deepgram's timing is noise rather than
/// evidence: it documents its streaming timestamps as not accurate below
/// millisecond precision, and an accepted end (`start + duration` in f64) and
/// the next reported start are independently rounded numbers. This is used in
/// exactly one place — [`is_fully_covered`]'s too-short-to-classify guard,
/// which only ever declines a suppression. No comparison anywhere adds it to
/// widen what counts as covered. See the module doc's "Suppressing a
/// re-emitted segment" section.
const SPAN_TOLERANCE_SECS: f64 = 0.001;

/// True only when every part of `new_span` already falls inside the audio
/// `accepted` covers, treating the accepted spans as one coverage set so a
/// re-emission spanning two adjacent accepted segments is recognised. A
/// segment that runs on past that coverage in either direction — a revised
/// or merged boundary carrying speech nobody has transcribed yet — is not
/// covered, and is kept whole; so is a span at or under
/// [`SPAN_TOLERANCE_SECS`] long, which is too short to classify.
///
/// Both comparisons below are exact, with no slack added on either side: a
/// gap in coverage is never forgiven, and an end that exceeds the coverage by
/// any amount at all keeps the segment, while an end landing exactly on it is
/// covered. That is what makes the tolerance unable to reach outward and
/// swallow audio nobody transcribed.
fn is_fully_covered(new_span: (f64, f64), mut accepted: Vec<(f64, f64)>) -> bool {
    if new_span.1 - new_span.0 <= SPAN_TOLERANCE_SECS {
        return false;
    }
    accepted.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut covered_to = new_span.0;
    for (start, end) in accepted {
        if start > covered_to {
            return false;
        }
        if end > covered_to {
            covered_to = end;
        }
        if covered_to >= new_span.1 {
            return true;
        }
    }
    false
}

/// Accumulates Deepgram's segmented results into one transcript.
///
/// Deepgram sends interim hypotheses for the current segment and then a final
/// version of it; finalised segments never change again, so the running
/// transcript is `finals + current interim`.
#[derive(Default)]
struct Transcript {
    finals: Vec<FinalSegment>,
    interim: String,
    done: bool,
    /// Set once a Results message arrives tagged `"from_finalize": true` —
    /// Deepgram's own authoritative confirmation that it produced this
    /// result in response to our `Finalize`. Cleared by the pump when it
    /// sends `Finalize`, so it only ever answers for that `Finalize`. See the
    /// module doc.
    finalize_acked: bool,
}

impl Transcript {
    /// Feed one JSON message. Returns the updated running transcript when the
    /// message changed it.
    fn absorb(&mut self, json: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;

        match value.get("type").and_then(|t| t.as_str()) {
            Some("Metadata") => {
                // Deepgram sends Metadata at open and again after CloseStream.
                // `done` is sticky here; the pump clears it unless CloseStream
                // has actually been sent (`was_closed_stream`), so only the
                // post-CloseStream frame ends the loop — a Metadata frame
                // that arrives merely after Finalize/`closing` started is not
                // enough, since CloseStream itself may not be out yet.
                self.done = true;
                return None;
            }
            Some("Results") | None => {}
            _ => return None,
        }

        if value.get("from_finalize").and_then(|v| v.as_bool()) == Some(true) {
            self.finalize_acked = true;
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
            let mut suppressed = false;
            if !text.is_empty() {
                let span = parse_span(&value);
                // A `None` span is never covered by anything, so missing or
                // unusable timing always falls through to being kept.
                let covered =
                    span.filter(|new_span| is_fully_covered(*new_span, self.accepted_spans()));
                match covered {
                    Some((start, end)) => {
                        vlog!(
                            "deepgram: withholding a re-emitted segment covering \
                             [{start:.3}s, {end:.3}s], already transcribed: {text:?}"
                        );
                        suppressed = true;
                    }
                    None => self.finals.push(FinalSegment { text, span }),
                }
            }
            // A re-emission of already-accepted audio says nothing about the
            // later segment the interim is accumulating, so it is left alone.
            if !suppressed {
                self.interim.clear();
            }
        } else {
            if text.is_empty() || text == self.interim {
                return None;
            }
            self.interim = text;
        }
        Some(self.running_text())
    }

    /// The audio the accepted segments cover, for segments Deepgram reported
    /// usable timing for.
    fn accepted_spans(&self) -> Vec<(f64, f64)> {
        self.finals.iter().filter_map(|seg| seg.span).collect()
    }

    fn running_text(&self) -> String {
        let mut parts: Vec<&str> = self.finals.iter().map(|seg| seg.text.as_str()).collect();
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

    #[test]
    fn the_outer_wait_never_sits_below_the_connect_budget() {
        // The invariant, not the arithmetic: latency may not be bought with
        // the user's words, and a connect killed inside its own budget loses
        // the whole utterance because nothing has streamed yet to salvage.
        // Two shapes satisfy it — a final wait long enough to contain this
        // engine's connect, or a published budget that `Dictation::finish`
        // waits out on the connect-only path — and either is a legitimate
        // answer. What is not legitimate is neither, which is what this
        // catches. `fm/iris-silent-and-instant` moves the same constant from
        // another direction; if this test is the conflict, read
        // `DEFAULT_FINAL_TIMEOUT`'s doc comment before resolving it.
        let engine = DeepgramEngine {
            key: "x".into(),
            url: DEFAULT_BASE_URL.into(),
        };
        assert!(
            engine.connect_budget() == Some(CONNECT_TIMEOUT)
                || engine.final_timeout() >= CONNECT_TIMEOUT,
            "the outer bound ({:?}) undercuts the connect budget ({CONNECT_TIMEOUT:?}) and \
             nothing covers the gap: connect_budget() = {:?}",
            engine.final_timeout(),
            engine.connect_budget(),
        );
    }

    fn results(text: &str, is_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "channel": { "alternatives": [ { "transcript": text } ] }
        })
        .to_string()
    }

    fn results_from_finalize(text: &str, from_finalize: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": true,
            "from_finalize": from_finalize,
            "channel": { "alternatives": [ { "transcript": text } ] }
        })
        .to_string()
    }

    /// A final Results message carrying the top-level `start`/`duration`
    /// timing real Deepgram traffic includes — see `parse_span`.
    fn results_with_span(text: &str, start: f64, duration: f64) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": true,
            "start": start,
            "duration": duration,
            "channel": { "alternatives": [ { "transcript": text } ] }
        })
        .to_string()
    }

    /// The same, but tagged `from_finalize` — for pinning that the dedup
    /// guard applies just as much to results the post-`Finalize` catch-up
    /// mechanism accepts as to ordinary mid-stream ones.
    fn results_from_finalize_with_span(text: &str, start: f64, duration: f64) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": true,
            "from_finalize": true,
            "start": start,
            "duration": duration,
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
    fn from_finalize_flag_is_tracked() {
        let mut t = Transcript::default();
        assert!(!t.finalize_acked);
        t.absorb(&results("hello", false));
        assert!(
            !t.finalize_acked,
            "an ordinary interim must not look like a finalize ack"
        );
        t.absorb(&results_from_finalize("hello there.", true));
        assert!(t.finalize_acked);
    }

    #[test]
    fn from_finalize_is_tracked_even_on_an_empty_result() {
        // Live-verified: a hold containing only silence still gets a
        // from_finalize-tagged result, just with no text in it.
        let mut t = Transcript::default();
        t.absorb(&results_from_finalize("", true));
        assert!(t.finalize_acked);
        assert_eq!(t.finished_text(), "");
    }

    #[test]
    fn a_reemitted_segment_covering_an_accepted_span_is_suppressed() {
        let mut t = Transcript::default();
        t.absorb(&results_with_span("the quick brown fox", 0.0, 1.5));
        // Deepgram re-sends the same audio span a second time.
        t.absorb(&results_with_span("the quick brown fox", 0.2, 1.0));
        assert_eq!(t.finished_text(), "the quick brown fox");
    }

    #[test]
    fn containment_suppresses_a_reemission_even_with_revised_wording() {
        // The discriminator is the span, not the text: a re-emission whose
        // audio is already covered is suppressed even when Deepgram revised
        // its wording for it, because accepting it would mean replacing
        // already-shown text — out of scope for a guard whose only job is not
        // to double it. See the module doc.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("the quick brown fox", 0.0, 1.5));
        t.absorb(&results_with_span("the quick brown", 0.1, 1.0));
        assert_eq!(t.finished_text(), "the quick brown fox");
    }

    #[test]
    fn a_reemission_merging_two_accepted_segments_is_suppressed() {
        // Accepted coverage is one set, not a list of unrelated spans: a
        // final that re-covers two adjacent accepted segments at once adds no
        // audio nobody has transcribed.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("the quick", 0.0, 0.8));
        t.absorb(&results_with_span("brown fox", 0.8, 0.7));
        t.absorb(&results_with_span("the quick brown fox", 0.0, 1.5));
        assert_eq!(t.finished_text(), "the quick brown fox");
    }

    #[test]
    fn a_segment_running_past_accepted_coverage_is_kept_whole() {
        // A revised or merged boundary that starts inside accepted audio and
        // then runs on into speech nobody has transcribed: keeping it whole
        // may duplicate a word at the seam, and that is the cheaper error.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("the quick brown fox", 0.0, 1.5));
        t.absorb(&results_with_span("fox jumps over the lazy dog", 1.4, 3.6));
        assert_eq!(
            t.finished_text(),
            "the quick brown fox fox jumps over the lazy dog"
        );
    }

    #[test]
    fn a_segment_starting_before_accepted_coverage_is_kept_whole() {
        // The same rule in the other direction: audio ahead of the earliest
        // accepted span is not covered, so the segment carrying it survives.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("brown fox", 1.0, 1.0));
        t.absorb(&results_with_span("the quick brown fox", 0.0, 1.6));
        assert_eq!(t.finished_text(), "brown fox the quick brown fox");
    }

    #[test]
    fn a_segment_running_past_coverage_by_less_than_the_tolerance_is_still_kept() {
        // The tolerance is never added to what counts as covered: an end that
        // exceeds the accepted coverage by even a fraction of a millisecond
        // keeps the segment. This test fails if that direction is flipped.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("hello there", 0.0, 1.5));
        let overshoot = SPAN_TOLERANCE_SECS / 5.0;
        t.absorb(&results_with_span("hello there", 0.5, 1.0 + overshoot));
        assert_eq!(t.finished_text(), "hello there hello there");
    }

    #[test]
    fn genuinely_repeated_word_in_adjacent_spans_survives_even_after_finalize() {
        // The case this branch exists for: two genuine "No."s, both arriving
        // after Finalize (the short-hold shape where a prior, since-removed
        // guard could not tell this apart from a re-emission). Adjacent spans
        // must not be suppressed.
        let mut t = Transcript::default();
        t.absorb(&results_from_finalize_with_span("No.", 0.0, 0.4));
        t.absorb(&results_from_finalize_with_span("No.", 0.4, 0.4));
        assert_eq!(t.finished_text(), "No. No.");
    }

    #[test]
    fn a_hairline_float_overlap_at_a_segment_boundary_keeps_both_words() {
        // The same "No. No.", but with the boundary reported as two
        // independently rounded numbers: the first segment's end computes to
        // 0.4400001 while the second starts at exactly 0.44. That hair of
        // overlap is rounding noise, not shared audio, and must not delete
        // the second word.
        let mut t = Transcript::default();
        t.absorb(&results_from_finalize_with_span("No.", 0.0, 0.4400001));
        t.absorb(&results_from_finalize_with_span("No.", 0.44, 0.44));
        assert_eq!(t.finished_text(), "No. No.");
    }

    #[test]
    fn a_suppressed_reemission_leaves_an_in_flight_interim_alone() {
        // A re-emission of already-accepted audio says nothing about the
        // later segment the interim is accumulating, and `finished_text`
        // reports that interim — clearing it here would delete the tail.
        let mut t = Transcript::default();
        t.absorb(&results_with_span("hello", 0.0, 1.5));
        t.absorb(&results("world", false));
        t.absorb(&results_with_span("hello", 0.2, 1.0));
        assert_eq!(t.finished_text(), "hello world");
    }

    #[test]
    fn missing_timing_data_is_never_suppressed() {
        // No start/duration on either message: the evidence needed to prove
        // a re-emission is absent, so both are kept even though the text is
        // identical.
        let mut t = Transcript::default();
        t.absorb(&results("hello", true));
        t.absorb(&results("hello", true));
        assert_eq!(t.finished_text(), "hello hello");
    }

    #[test]
    fn non_positive_duration_is_treated_as_unusable_timing() {
        let mut t = Transcript::default();
        t.absorb(&results_with_span("hi", 0.0, 1.0));
        // A zero-duration span asserts nothing about what it covers, so it
        // must not be read as proof of overlap.
        t.absorb(&results_with_span("hi", 0.0, 0.0));
        assert_eq!(t.finished_text(), "hi hi");
    }

    #[test]
    fn negative_start_is_treated_as_unusable_timing() {
        let mut t = Transcript::default();
        t.absorb(&results_with_span("hi", 0.0, 1.0));
        t.absorb(&results_with_span("hi", -1.0, 1.0));
        assert_eq!(t.finished_text(), "hi hi");
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
    fn no_audio_sent_blames_the_hold_not_the_microphone() {
        // Symptom B: a hold so short nothing was ever fed to the engine. The
        // old message ("silence, or audio was not reaching the mic") sent
        // users chasing a hardware problem that did not exist.
        match conclude(&Transcript::default(), 0.0, None) {
            TranscriptEvent::Error(msg) => {
                assert!(!msg.to_lowercase().contains("mic"), "{msg}");
                assert!(msg.contains("released") || msg.contains("longer"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn audio_sent_but_nothing_transcribed_reads_as_silence_not_a_hold_problem() {
        // Real audio reached Deepgram (unlike the case above), it acknowledged
        // the flush, and it just had nothing to say about it — a genuinely
        // different situation that must not share a message with the
        // zero-audio case.
        let mut t = Transcript::default();
        t.absorb(&results_from_finalize("", true));
        assert!(t.finalize_acked);
        match conclude(&t, 2.5, None) {
            TranscriptEvent::Error(msg) => {
                assert!(msg.contains("silence"), "{msg}");
                assert!(!msg.contains("released"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_finalize_ack_is_not_reported_as_silence() {
        // The safety-net path: real audio went out, nothing came back, and
        // Deepgram never acknowledged the flush. Claiming it "heard the audio"
        // and blaming silence is exactly the misleading diagnosis this branch
        // set out to remove — the engine confirmed nothing at all.
        match conclude(&Transcript::default(), 2.5, None) {
            TranscriptEvent::Error(msg) => {
                assert!(!msg.contains("silence"), "{msg}");
                assert!(!msg.to_lowercase().contains("mic"), "{msg}");
                assert!(msg.contains("confirm"), "{msg}");
                assert!(msg.contains("try again"), "{msg}");
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

    /// Runs `pump_inner` against a local fake server that behaves like real
    /// Deepgram for exactly the shape under test, with a short
    /// `finalize_ack_timeout` so a safety-net test does not have to burn the
    /// real, generous production value.
    async fn run_pump(
        audio: UnboundedReceiver<Command>,
        events: Sender<TranscriptEvent>,
        addr: std::net::SocketAddr,
        finalize_ack_timeout: Duration,
    ) {
        pump_inner(
            format!("ws://{addr}"),
            "test-key".into(),
            audio,
            events,
            finalize_ack_timeout,
            None,
        )
        .await
        .unwrap();
    }

    /// The fake servers' socket type — spelled out once so [`wait_for`] can
    /// borrow it without every server body naming it.
    type FakeSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    /// Read frames until one contains `needle`, ignoring anything else (audio
    /// binaries, pings). Returns false if the socket ended first, which every
    /// caller treats as "the client went away, stop scripting".
    async fn wait_for(ws: &mut FakeSocket, needle: &str) -> bool {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) if t.contains(needle) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    }

    /// How long the fake server watches for a rushed `CloseStream` after
    /// `Finalize`. Long enough that a client racing ahead is caught, short
    /// enough that a correctly-waiting client (which sends nothing until it
    /// has the ack) does not slow the test down.
    const RUSH_WINDOW: Duration = Duration::from_millis(80);

    /// A minimal, adversarial fake Deepgram: it withholds any response until
    /// `Finalize` arrives (nothing streamed back during the hold — exactly a
    /// hold shorter than connect-plus-first-result), then only hands over the
    /// `from_finalize`-tagged result if the client did *not* rush to close
    /// the stream. A client that sends `CloseStream` immediately after
    /// `Finalize` gets an early Metadata sign-off and permanently loses the
    /// flush — reproducing the real bug on a real, if fake, wire rather than
    /// a hand-written two-message fixture.
    ///
    /// Every hold shape below drives this same server; the only thing that
    /// varies is the audio fed in, the transcript and `duration` it reports
    /// back, and the [`Quirk`] it plays after the flush.
    fn spawn_withhold_then_flush_server(
        listener: tokio::net::TcpListener,
        reply_text: &'static str,
        metadata_duration: f64,
        quirk: Quirk,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let metadata = serde_json::json!({ "type": "Metadata", "duration": metadata_duration })
                .to_string();
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            if !wait_for(&mut ws, "Finalize").await {
                return;
            }

            if quirk == Quirk::LateOpenMetadata {
                // The open-time Metadata frame, delayed until the hold is
                // already over — the whole press can fit inside the connect
                // window, so the client may not have polled the socket once
                // before Finalize went out. It is *not* the sign-off, and a
                // client that treats it as one loses the flush below.
                ws.send(Message::Text(metadata.clone().into()))
                    .await
                    .unwrap();
            }

            // Did the client rush to close instead of waiting for the flush?
            let rushed = tokio::time::timeout(RUSH_WINDOW, ws.next()).await.is_ok();
            if rushed {
                let _ = ws.send(Message::Text(metadata.into())).await;
                return;
            }

            ws.send(Message::Text(
                results_from_finalize(reply_text, true).into(),
            ))
            .await
            .unwrap();

            if quirk == Quirk::GarbageAfterFlush {
                // A well-formed text frame (FIN, opcode 1, 2 unmasked bytes)
                // whose payload is not UTF-8 — a genuine, deterministic socket
                // error landing *after* the flush, which must not displace the
                // text already handed over.
                use tokio::io::AsyncWriteExt;
                ws.get_mut()
                    .write_all(&[0x81, 0x02, 0xFF, 0xFF])
                    .await
                    .unwrap();
                let _ = ws.get_mut().flush().await;
                return;
            }

            if !wait_for(&mut ws, "CloseStream").await {
                return;
            }
            if let Quirk::DelayedSignOff(delay) = quirk {
                // Stall the sign-off so a test can tell that Final really is
                // gated on this frame and not on the flush before it.
                tokio::time::sleep(delay).await;
            }
            let _ = ws.send(Message::Text(metadata.into())).await;
        })
    }

    /// What [`spawn_withhold_then_flush_server`] does around the flush, beyond
    /// the plain withhold-then-flush behaviour every case shares.
    #[derive(Clone, Copy, PartialEq)]
    enum Quirk {
        None,
        /// Emit the open-time Metadata after `Finalize` but before the flush.
        LateOpenMetadata,
        /// Wait before the post-`CloseStream` Metadata sign-off.
        DelayedSignOff(Duration),
        /// Break the socket right after the flush.
        GarbageAfterFlush,
    }

    /// A fake Deepgram that answers `Finalize` with nothing at all — no
    /// `from_finalize`, no results — and only signs off once the client gives
    /// up and sends `CloseStream` on its own. Covers both the protocol-failure
    /// safety net and the zero-audio short-circuit, which differ in *why* the
    /// client closes without an ack, not in what the server does.
    fn spawn_silent_until_close_server(
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            if !wait_for(&mut ws, "Finalize").await {
                return;
            }
            if !wait_for(&mut ws, "CloseStream").await {
                return;
            }
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"Metadata","duration":1.0}"#.into(),
                ))
                .await;
        })
    }

    /// Drain everything the pump has emitted so far, appending to `seen`.
    fn drain(events: &Receiver<TranscriptEvent>, seen: &mut Vec<TranscriptEvent>) {
        seen.extend(events.try_iter());
    }

    fn finals(seen: &[TranscriptEvent]) -> Vec<&str> {
        seen.iter()
            .filter_map(|e| match e {
                TranscriptEvent::Final(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_hold_shorter_than_first_response_does_not_abandon_the_backlog() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(
            listener,
            "It's not working correctly. I've been having trouble.",
            3.0,
            Quirk::None,
        );

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // ~3s of "audio" — content is irrelevant off a real mic; only the
        // byte count (what `sent_secs` tracks) matters to the fix under test.
        let three_seconds = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2 * 3];
        audio_tx.send(Command::Audio(three_seconds)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        run_pump(audio_rx, event_tx.clone(), addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["It's not working correctly. I've been having trouble."],
            "CloseStream must wait for from_finalize, not race ahead of it"
        );
    }

    /// The captain's own `short-hold-bypasses-catchup` finding: the prior
    /// mechanism never engaged at all for a hold at or under its own rounding
    /// tolerance, reopening the exact bug this module exists to close for a
    /// quick press. This drives a genuinely short hold (well under 0.5s, the
    /// old tolerance) through the same adversarial withhold-then-flush server
    /// to prove the new mechanism has no such blind spot.
    #[tokio::test]
    async fn a_hold_under_half_a_second_still_waits_for_the_flush() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(listener, "No.", 0.3, Quirk::None);

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // 0.3s: well under the old CATCHUP_TOLERANCE_SECS (0.5s) that used to
        // make the wait never engage at all.
        let short_hold = vec![0u8; (crate::audio::SAMPLE_RATE as f64 * 0.3) as usize * 2];
        audio_tx.send(Command::Audio(short_hold)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        run_pump(audio_rx, event_tx.clone(), addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["No."],
            "a hold under 0.5s must still wait for the from_finalize flush, not skip waiting"
        );
    }

    /// `negligible-audio-threshold-skips-wait`: `NEGLIGIBLE_AUDIO_SECS` (0.1s)
    /// must never gate the wait itself — only `sent_secs == 0.0` may. Live
    /// evidence showed Deepgram acks a real ~50ms hold, so this drives that
    /// exact duration (comfortably inside the old 0.1s threshold, and an
    /// order of magnitude under the 0.5s one) through the same
    /// withhold-then-flush server and requires the flush to still be waited
    /// for, not skipped as "basically no audio".
    #[tokio::test]
    async fn a_50ms_hold_still_waits_for_the_flush() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(listener, "Hi.", 0.05, Quirk::None);

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // ~50ms: the exact duration live-verified (near_silence.wav) to still
        // get a from_finalize ack, well inside NEGLIGIBLE_AUDIO_SECS (0.1s) —
        // the threshold that must select error wording only, never gate this
        // wait.
        let fifty_ms = vec![0u8; (crate::audio::SAMPLE_RATE as f64 * 0.05) as usize * 2];
        audio_tx.send(Command::Audio(fifty_ms)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        run_pump(audio_rx, event_tx.clone(), addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["Hi."],
            "50ms of real audio is not zero audio and must still wait for from_finalize"
        );
    }

    /// Nothing carries `from_finalize` within the safety net — a malformed or
    /// missing signal, the one case the timeout exists for. The session must
    /// still close (via the ordinary post-`CloseStream` Metadata wait)
    /// instead of hanging, salvaging whatever text arrived beforehand.
    #[tokio::test]
    async fn a_missing_finalize_ack_closes_anyway_after_the_safety_net() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_silent_until_close_server(listener);

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        let started = Instant::now();
        // A short timeout here so the test does not have to spend the real,
        // generous production value to prove the safety net fires.
        run_pump(audio_rx, event_tx.clone(), addr, Duration::from_millis(200)).await;
        server.await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the safety net must bound the wait, not hang: took {:?}",
            started.elapsed()
        );
        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        let error = seen.iter().find_map(|e| match e {
            TranscriptEvent::Error(m) => Some(m.as_str()),
            _ => None,
        });
        let Some(error) = error else {
            panic!(
                "nothing was ever transcribed, so this must conclude as an error, \
                 not silently succeed: {seen:?}"
            );
        };
        assert!(
            !error.contains("silence"),
            "the engine confirmed nothing; blaming silence is the misleading \
             diagnosis this branch removes: {error}"
        );
    }

    /// `untested-closed-stream-disambiguation`: the whole hold can fit inside
    /// the connect window, so Deepgram's *open-time* Metadata can land after
    /// `Finalize` but before `CloseStream` goes out. That frame is not the
    /// sign-off, and a client that ends the session on it (gating on `closing`
    /// rather than on `closed_stream`) loses the entire flush. No other server
    /// variant reaches this window, so without this the gate could regress
    /// silently.
    #[tokio::test]
    async fn a_late_open_metadata_is_not_mistaken_for_the_sign_off() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(
            listener,
            "Late but not last.",
            1.0,
            Quirk::LateOpenMetadata,
        );

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        run_pump(audio_rx, event_tx, addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["Late but not last."],
            "a Metadata frame arriving before CloseStream must not end the session"
        );
    }

    /// The `sent_secs == 0.0` short-circuit, at the pump level rather than
    /// only inside `conclude`: with nothing ever sent there is no flush for
    /// Deepgram to tag, so `CloseStream` must go out immediately instead of
    /// burning the full (real, three-second) safety net waiting for a signal
    /// that cannot come.
    #[tokio::test]
    async fn a_session_with_no_audio_closes_without_waiting_for_an_ack() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_silent_until_close_server(listener);

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        // No Command::Audio at all — the key came up before a single frame
        // was captured.
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        let started = Instant::now();
        run_pump(audio_rx, event_tx, addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "zero audio must skip the {FINALIZE_ACK_TIMEOUT:?} wait entirely: took {:?}",
            started.elapsed()
        );
        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        match seen.iter().find(|e| matches!(e, TranscriptEvent::Error(_))) {
            Some(TranscriptEvent::Error(msg)) => {
                assert!(msg.contains("released") || msg.contains("longer"), "{msg}")
            }
            _ => panic!("expected the zero-audio error, got {seen:?}"),
        }
    }

    /// Final is emitted once, on the session close, and deliberately *not*
    /// early on the `from_finalize` flush: an ack proves the flush started,
    /// not that it fit in one frame, and the module doc records why no quiet
    /// window after it can be both wide enough for the observed inter-message
    /// gaps and cheap enough for the latency budget. The server stalls the
    /// sign-off, so a Final that arrived before the stall was over could only
    /// have come from the abandoned early-emission path.
    #[tokio::test]
    async fn final_waits_for_the_sign_off_not_the_flush() {
        const SIGN_OFF_DELAY: Duration = Duration::from_millis(300);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(
            listener,
            "Complete, please.",
            1.0,
            Quirk::DelayedSignOff(SIGN_OFF_DELAY),
        );

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        let started = Instant::now();
        let pump = tokio::spawn(run_pump(audio_rx, event_tx, addr, FINALIZE_ACK_TIMEOUT));

        let mut seen = Vec::new();
        let final_at = loop {
            drain(&event_rx, &mut seen);
            if !finals(&seen).is_empty() {
                break started.elapsed();
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no Final ever arrived: {seen:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(
            final_at >= SIGN_OFF_DELAY,
            "Final is gated on the sign-off, so it cannot precede it: took {final_at:?}"
        );

        pump.await.unwrap();
        server.await.unwrap();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["Complete, please."],
            "the terminal event must still be emitted exactly once"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, TranscriptEvent::Error(_))),
            "a clean teardown must not add an error alongside the Final: {seen:?}"
        );
    }

    /// The failure-path precedence, pinned rather than left to inspection: a
    /// socket error *after* the flush must never displace the words Deepgram
    /// already handed over.
    #[tokio::test]
    async fn a_socket_failure_after_the_flush_does_not_displace_the_transcript() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_withhold_then_flush_server(
            listener,
            "Say this anyway.",
            1.0,
            Quirk::GarbageAfterFlush,
        );

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        run_pump(audio_rx, event_tx, addr, FINALIZE_ACK_TIMEOUT).await;
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["Say this anyway."],
            "the transcript takes priority over a socket error that follows it"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, TranscriptEvent::Error(_))),
            "a teardown failure must not be reported once the text is out: {seen:?}"
        );
    }

    /// A live warm handoff: `pump_inner` given an already-connected socket
    /// must use it directly, reporting `Connected` without a fresh connect
    /// and still completing the session normally.
    #[tokio::test]
    async fn a_live_warm_handoff_skips_connecting_and_still_completes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");
        let server =
            spawn_withhold_then_flush_server(listener, "From the warm spare.", 1.0, Quirk::None);

        let warm = connect(&url, "test-key").await.unwrap();

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        pump_inner(
            url,
            "test-key".into(),
            audio_rx,
            event_tx,
            FINALIZE_ACK_TIMEOUT,
            Some(warm),
        )
        .await
        .unwrap();
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert!(
            seen.iter().any(|e| matches!(e, TranscriptEvent::Connected)),
            "a warm handoff must still report Connected: {seen:?}"
        );
        assert_eq!(finals(&seen), ["From the warm spare."]);
    }

    /// A fake server that accepts and immediately drops its first connection
    /// — the shape of a warm spare that died in the gap between its last
    /// keep-alive and being handed to a real dictation — then serves the
    /// ordinary withhold-then-flush behaviour on the second connection,
    /// which `pump_inner`'s reconnect fallback must land on.
    fn spawn_dead_then_flush_server(
        listener: tokio::net::TcpListener,
        reply_text: &'static str,
        metadata_duration: f64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // A graceful close (a plain drop) only sends FIN, which a small
            // write can still succeed into the client's send buffer for —
            // TCP write() completing does not require peer acknowledgement.
            // SO_LINGER(0) forces RST on close instead, which the client's
            // next send observes as a prompt, reliable error rather than a
            // write that silently buffers into a connection nobody is
            // reading from. tokio deprecated this over drop() blocking on a
            // *nonzero* linger; zero is the immediate-RST, does-not-block
            // case the deprecation note itself does not apply to.
            #[allow(deprecated)]
            ws.get_ref().set_linger(Some(Duration::ZERO)).unwrap();
            drop(ws);
            // Give the loopback stack a moment to tear the connection down,
            // so the client's next send on it observes a real error instead
            // of racing a not-yet-processed close.
            tokio::time::sleep(Duration::from_millis(100)).await;

            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            if !wait_for(&mut ws, "Finalize").await {
                return;
            }
            ws.send(Message::Text(
                results_from_finalize(reply_text, true).into(),
            ))
            .await
            .unwrap();
            if !wait_for(&mut ws, "CloseStream").await {
                return;
            }
            let metadata = serde_json::json!({ "type": "Metadata", "duration": metadata_duration })
                .to_string();
            let _ = ws.send(Message::Text(metadata.into())).await;
        })
    }

    /// The critical safety property behind the whole warm-spare mechanism:
    /// a handed-in connection that has actually died must never cost the
    /// user their words. `pump_inner` reconnects cold and retries on the
    /// first send failure (`confirm_pending`); this drives that path against
    /// a real dead socket on a real (loopback) network and checks the
    /// transcript still arrives whole, exactly as if the spare had never
    /// been offered at all.
    #[tokio::test]
    async fn a_stale_warm_handoff_reconnects_without_losing_audio() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");

        // Spawn the server before connecting: nothing would answer the WS
        // handshake otherwise.
        let server = spawn_dead_then_flush_server(listener, "Recovered after reconnect.", 1.0);

        // Establish the "warm" socket against this same server; the server
        // then drops it before ever completing a real session on it.
        let warm = connect(&url, "test-key").await.unwrap();

        let (audio_tx, audio_rx) = unbounded_channel::<Command>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let one_second = vec![0u8; crate::audio::SAMPLE_RATE as usize * 2];
        audio_tx.send(Command::Audio(one_second)).unwrap();
        audio_tx.send(Command::Finish).unwrap();
        drop(audio_tx);

        pump_inner(
            url,
            "test-key".into(),
            audio_rx,
            event_tx,
            FINALIZE_ACK_TIMEOUT,
            Some(warm),
        )
        .await
        .unwrap();
        server.await.unwrap();

        let mut seen = Vec::new();
        drain(&event_rx, &mut seen);
        assert_eq!(
            finals(&seen),
            ["Recovered after reconnect."],
            "a dead warm handoff must transparently reconnect and still \
             deliver the transcript, never lose it: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, TranscriptEvent::Error(_))),
            "the reconnect must be invisible to the caller, not surfaced as \
             an error: {seen:?}"
        );
    }

    #[tokio::test]
    async fn warm_pool_try_take_is_empty_until_something_is_stored() {
        let pool = WarmPool::new();
        assert!(pool.try_take().is_none());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Hold the connection open for the test to observe try_take.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let socket = connect(&url, "test-key").await.unwrap();
        *pool.slot.lock().await = Some((socket, Instant::now()));

        assert!(pool.try_take().is_some());
        assert!(
            pool.try_take().is_none(),
            "taking the spare must not leave a second one behind"
        );
        server.abort();
    }
}
