//! The overlay state machine and its animation clock.
//!
//! This module is the headless heart of the crate: no window, no rasteriser, no
//! wall clock. Time is passed in as milliseconds since the overlay started, so
//! every transition and every easing curve is exercised deterministically by the
//! tests rather than by staring at a screen.

use crate::motion::{
    one_pole, EASE_IN, EASE_OUT, ENTER_MS, EXIT_MS, INSERTED_HOLD_MS, LEVEL_ATTACK_MS,
    LEVEL_RELEASE_MS, STATE_CROSS_MS,
};
use crate::theme::Theme;

/// The four states of the pill.
///
/// These are the states the *product* has. Entering and leaving is animation,
/// not a fifth state: `Hidden` is entered the instant the caller asks for it and
/// the pill spends [`EXIT_MS`] fading out afterwards, which is why
/// [`Model::is_idle`] exists separately from `state() == Hidden`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayState {
    /// Nothing on screen. The window is hidden and the loop is asleep.
    Hidden,
    /// The hotkey is down. The resting capsule (or open ribbon) tracks the
    /// microphone, the elapsed time, and the live transcript.
    Listening,
    /// The hotkey is up and the engine is finishing. The last text holds; a
    /// shimmer sweeps the open ribbon (or a spinner, if nothing was captured).
    Processing,
    /// Text has landed in the target application. The shape collapses back to
    /// its resting width, a check draws itself into it, then the pill leaves
    /// on its own.
    Inserted,
}

impl OverlayState {
    /// Whether the pill draws anything at all in this state.
    #[must_use]
    pub fn is_visible(self) -> bool {
        !matches!(self, OverlayState::Hidden)
    }
}

/// A message to the overlay thread.
///
/// [`crate::OverlayHandle`] is the ergonomic front for this; the enum is public
/// so a caller can drive a [`crate::HeadlessOverlay`] directly, or put the
/// overlay behind their own transport.
#[derive(Clone, Debug)]
pub enum Command {
    /// Show the pill and start listening.
    ShowListening,
    /// Report the current microphone level, 0.0–1.0. Out-of-range values are
    /// clamped rather than rejected.
    Level(f32),
    /// Report the current partial-transcript text.
    ///
    /// This is the overlay's one deliberate reversal of the earlier rule that
    /// it never holds transcript content — see `README.md` and the design
    /// report at `data/iris-ui-directions/report.md` for why. The overlay
    /// holds this string for exactly as long as it is on screen and never
    /// persists, transmits, or logs it.
    PartialText(String),
    /// Set the engine label, e.g. `groq · whisper-large-v3-turbo · en`.
    ///
    /// Carried on the model for continuity with callers that already send it,
    /// but nothing in this design renders it — the ribbon has no room for a
    /// second line of text without competing with the transcript for
    /// attention. Kept as live data rather than removed outright so a future
    /// direction can pick it back up without a contract change.
    Engine(String),
    /// Mark the current `Listening` session as a hands-free latch (recording
    /// continues with the hotkey released) or not.
    ///
    /// Orthogonal to the state machine below, the same way `Level` and
    /// `PartialText` are: it never moves `state`, only how a `Listening`
    /// pill is drawn (see `render::core_colour`/`render::glow_colour`), so it
    /// applies unconditionally regardless of the current state. Reset to
    /// `false` by a fresh `ShowListening`, the same way the transcript and
    /// the timer are, so a latch never bleeds into the next utterance if the
    /// caller ever forgot to send `Latched(false)` first.
    Latched(bool),
    /// The hotkey is up; the engine is finishing.
    Processing,
    /// Text landed. Shows the check and the end-to-end latency, then hides
    /// itself after [`INSERTED_HOLD_MS`].
    Inserted {
        /// End-to-end latency to print, in milliseconds.
        latency_ms: u32,
    },
    /// Hide the pill now, playing the exit animation.
    Hide,
    /// Switch palettes. Takes effect on the next frame; geometry is unchanged.
    ///
    /// Boxed because this channel also carries a [`Command::Level`] per audio
    /// frame, and an inline [`Theme`] would widen every one of those to the
    /// size of a whole palette.
    SetTheme(Box<Theme>),
    /// Stop the overlay thread.
    Shutdown,
}

/// The animated model behind one pill.
///
/// Drive it with [`Model::apply`] for commands and [`Model::tick`] once per
/// frame. Everything the renderer needs is a getter on this type.
///
/// This holds no shape or text-measurement state — no font, no width, no
/// morph progress. Those live on the `Renderer`, which owns the `FontAtlas`
/// they depend on. `Model` is deliberately shape-agnostic, the same way it
/// was when the pill was a fixed 168×34 capsule.
#[derive(Clone, Debug)]
pub struct Model {
    theme: Theme,
    state: OverlayState,
    previous: OverlayState,

    now_ms: u64,
    entered_at: u64,
    started: bool,

    /// Linear 0.0–1.0 presence: 0 is fully gone, 1 is fully arrived.
    presence: f32,

    level: f32,
    level_target: f32,

    text: String,
    engine: String,
    latency_ms: Option<u32>,
    latched: bool,

    listen_started_at: u64,
    listen_frozen_ms: Option<u64>,
}

impl Model {
    /// A fresh model in [`OverlayState::Hidden`].
    #[must_use]
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            state: OverlayState::Hidden,
            previous: OverlayState::Hidden,
            now_ms: 0,
            entered_at: 0,
            started: false,
            presence: 0.0,
            level: 0.0,
            level_target: 0.0,
            text: String::new(),
            engine: String::new(),
            latency_ms: None,
            latched: false,
            listen_started_at: 0,
            listen_frozen_ms: None,
        }
    }

    // -- commands ----------------------------------------------------------

    /// Apply one command.
    ///
    /// Transitions are deliberately narrow, because the caller is an audio
    /// pipeline that can and will emit events out of order under load:
    ///
    /// | from \ command | `ShowListening` | `Processing` | `Inserted` | `Hide` |
    /// |---|---|---|---|---|
    /// | `Hidden`     | → `Listening`      | ignored      | ignored    | ignored |
    /// | `Listening`  | ignored (no restart) | → `Processing` | → `Inserted` | → `Hidden` |
    /// | `Processing` | → `Listening`      | ignored      | → `Inserted` | → `Hidden` |
    /// | `Inserted`   | → `Listening`      | ignored      | → `Inserted` (refresh) | → `Hidden` |
    ///
    /// The two ignores that matter: `Processing` and `Inserted` from `Hidden`
    /// do nothing, so a stray engine event after the user cancelled cannot
    /// flash a pill onto a screen the user had already dismissed.
    ///
    /// Returns `false` for [`Command::Shutdown`], and `true` otherwise, so a
    /// run loop can use it directly as its "keep going" signal.
    pub fn apply(&mut self, command: Command) -> bool {
        use OverlayState::{Hidden, Inserted, Listening, Processing};
        match command {
            Command::ShowListening => {
                if self.state != Listening {
                    self.latency_ms = None;
                    self.text.clear();
                    self.level = 0.0;
                    self.level_target = 0.0;
                    self.latched = false;
                    self.listen_started_at = self.now_ms;
                    self.listen_frozen_ms = None;
                    self.enter(Listening);
                }
            }
            Command::Level(level) => {
                self.level_target = if level.is_finite() {
                    level.clamp(0.0, 1.0)
                } else {
                    0.0
                };
            }
            Command::PartialText(text) => self.text = text,
            Command::Engine(label) => self.engine = label,
            Command::Latched(on) => self.latched = on,
            Command::Processing => {
                if matches!(self.state, Listening) {
                    self.freeze_timer();
                    self.enter(Processing);
                }
            }
            Command::Inserted { latency_ms } => {
                if self.state.is_visible() {
                    self.freeze_timer();
                    self.latency_ms = Some(latency_ms);
                    self.enter(Inserted);
                }
            }
            Command::Hide => {
                if self.state != Hidden {
                    self.freeze_timer();
                    self.enter(Hidden);
                }
            }
            Command::SetTheme(theme) => self.theme = *theme,
            Command::Shutdown => return false,
        }
        true
    }

    fn enter(&mut self, state: OverlayState) {
        self.previous = self.state;
        self.state = state;
        self.entered_at = self.now_ms;
    }

    fn freeze_timer(&mut self) {
        if self.state == OverlayState::Listening && self.listen_frozen_ms.is_none() {
            self.listen_frozen_ms = Some(self.now_ms.saturating_sub(self.listen_started_at));
        }
    }

    // -- clock -------------------------------------------------------------

    /// Advance one frame. `now_ms` is milliseconds since the overlay started.
    ///
    /// A frame longer than 100 ms is treated as 100 ms: after the loop has been
    /// parked on the idle poll, or the machine has been asleep, a literal delta
    /// would teleport every animation to its end state.
    pub fn tick(&mut self, now_ms: u64) {
        let dt = if self.started {
            (now_ms.saturating_sub(self.now_ms) as f32).min(100.0)
        } else {
            self.started = true;
            0.0
        };
        self.now_ms = now_ms;

        // The one automatic transition: inserted times itself out.
        if self.state == OverlayState::Inserted
            && now_ms.saturating_sub(self.entered_at) >= u64::from(INSERTED_HOLD_MS)
        {
            self.enter(OverlayState::Hidden);
        }

        // Presence walks toward 1 while visible and 0 while hidden, at the
        // enter and exit rates respectively.
        let duration = if self.state.is_visible() {
            ENTER_MS
        } else {
            EXIT_MS
        } as f32;
        let step = if duration > 0.0 { dt / duration } else { 1.0 };
        self.presence = if self.state.is_visible() {
            (self.presence + step).min(1.0)
        } else {
            (self.presence - step).max(0.0)
        };

        // Microphone level: quick to rise, slow to fall.
        let tau = if self.level_target > self.level {
            LEVEL_ATTACK_MS
        } else {
            LEVEL_RELEASE_MS
        };
        self.level += (self.level_target - self.level) * one_pole(tau, dt);
    }

    // -- what the renderer and the run loop read ---------------------------

    /// The current state.
    #[must_use]
    pub fn state(&self) -> OverlayState {
        self.state
    }

    /// The state the pill was in before this one.
    #[must_use]
    pub fn previous_state(&self) -> OverlayState {
        self.previous
    }

    /// The active palette.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Milliseconds since the overlay started, as of the last [`Model::tick`].
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// Milliseconds spent in the current state.
    #[must_use]
    pub fn age_ms(&self) -> u64 {
        self.now_ms.saturating_sub(self.entered_at)
    }

    /// Nothing to draw and nothing left to animate: the window can be hidden
    /// and the loop can park on its idle poll.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.state == OverlayState::Hidden && self.presence <= 0.0
    }

    /// How present the pill is, 0.0–1.0, with the enter/exit easing applied.
    ///
    /// Drives opacity, the 10 px rise and the 0.90 → 1.0 scale, which is the
    /// complete list of properties the enter and exit animations touch. The
    /// report is explicit that this is transform and opacity only.
    #[must_use]
    pub fn presence(&self) -> f32 {
        if self.state.is_visible() {
            EASE_IN.eval(self.presence)
        } else {
            1.0 - EASE_OUT.eval(1.0 - self.presence)
        }
    }

    /// Progress of the cross-fade into the current state, 0.0–1.0.
    #[must_use]
    pub fn cross(&self) -> f32 {
        if STATE_CROSS_MS == 0 {
            return 1.0;
        }
        let progress = self.age_ms() as f32 / STATE_CROSS_MS as f32;
        EASE_IN.eval(progress.clamp(0.0, 1.0))
    }

    /// The smoothed microphone level, 0.0–1.0.
    ///
    /// Silence at the start of every dictation: a fresh
    /// [`Command::ShowListening`] resets both this and the target it is
    /// smoothing towards, the same way it clears the text and the timer.
    /// Nothing else can — [`Command::Level`] is the only other writer, the
    /// audio thread stops sending it the moment a hold ends, and the run loop
    /// keeps calling [`Model::tick`] while idle, so without this reset the
    /// level would sit on the last utterance's loudness indefinitely and the
    /// next appearance would open reading that instead of the room.
    #[must_use]
    pub fn level(&self) -> f32 {
        self.level
    }

    /// The current partial-transcript text. Empty before any has arrived, or
    /// once cleared by a fresh [`Command::ShowListening`].
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The engine label. Carried but not rendered by this design — see
    /// [`Command::Engine`].
    #[must_use]
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// Whether the current `Listening` session is a hands-free latch. See
    /// [`Command::Latched`].
    #[must_use]
    pub fn latched(&self) -> bool {
        self.latched
    }

    /// The latency to print, once one has been reported.
    #[must_use]
    pub fn latency_ms(&self) -> Option<u32> {
        self.latency_ms
    }

    /// How long the user has been speaking, in milliseconds. Frozen when
    /// listening ends so the readout does not keep counting during processing.
    #[must_use]
    pub fn listening_ms(&self) -> u64 {
        self.listen_frozen_ms
            .unwrap_or_else(|| self.now_ms.saturating_sub(self.listen_started_at))
    }

    /// The offset and scale the whole pill is drawn with, given `presence`.
    ///
    /// Returns `(dy, scale)`: the pill rises 10 logical pixels and grows from
    /// 90 % as it arrives, about its own bottom edge.
    #[must_use]
    pub fn enter_transform(&self, layout_scale: f32) -> (f32, f32) {
        let p = self.presence();
        ((1.0 - p) * 10.0 * layout_scale, 0.90 + 0.10 * p)
    }
}

/// The longest elapsed time the readout shows, in seconds. Past it the run
/// saturates rather than growing a fifth character — see [`format_timer`].
const TIMER_CEILING_S: u64 = 9 * 60 + 59;

/// Format elapsed milliseconds as `m:ss`, the way the mockup's timer read,
/// saturating at [`TIMER_CEILING_S`].
///
/// This is the pill's timer: `render::Renderer::draw` calls it every frame on
/// [`Model::listening_ms`] and `render::draw_timer` puts the result on screen
/// beside the wave row (captain, round 3: "the timer beside it, of course").
/// It went a round unrendered — the ribbon showed words there instead — which
/// is why the round-3 timer needed no new state machinery, only a draw step.
///
/// The ceiling is a *layout* guarantee, not a claim about the recording. The
/// resting capsule is narrow by decision, and `render::timer_right_edge`
/// reserves the run's width away from the centred glyph's ink; a readout that
/// could grow a character at ten minutes would spend that reservation and
/// draw its leading digit through the spinner or the check. Bounding the
/// format is what makes the reserved width the width the run can ever have.
/// A dictation held past ten continuous minutes is far outside what this
/// surface is for — the number stops being informative long before it stops
/// being true, and `format_timer(0)`'s four characters are what the geometry
/// is built around.
///
/// Reviewed and kept: the alternative is reserving a fifth character for a
/// reading nobody will reach, which spends about 9 logical pixels of wave row
/// out of a capsule that is narrow by decision, on every dictation, forever.
/// A readout that stops climbing in the tenth minute is the accepted cost of
/// that trade, not an oversight — if the ceiling ever has to move, widen
/// `render::timer_right_edge`'s reservation in the same change or the leading
/// digit lands on the centred glyph.
#[must_use]
pub fn format_timer(ms: u64) -> String {
    let total = (ms / 1000).min(TIMER_CEILING_S);
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{PORCELAIN_LIGHT, PRISM_DARK};

    fn model() -> Model {
        let mut m = Model::new(PRISM_DARK);
        m.tick(0);
        m
    }

    /// Run the model forward to `until_ms` at 60 fps.
    fn run_to(m: &mut Model, from_ms: u64, until_ms: u64) {
        let mut t = from_ms;
        while t < until_ms {
            t = (t + 16).min(until_ms);
            m.tick(t);
        }
    }

    #[test]
    fn starts_hidden_and_idle() {
        let m = model();
        assert_eq!(m.state(), OverlayState::Hidden);
        assert!(m.is_idle());
        assert_eq!(m.presence(), 0.0);
        assert_eq!(m.text(), "");
    }

    #[test]
    fn the_happy_path_walks_all_four_states() {
        let mut m = model();

        m.apply(Command::ShowListening);
        assert_eq!(m.state(), OverlayState::Listening);
        assert!(!m.is_idle());

        m.apply(Command::Processing);
        assert_eq!(m.state(), OverlayState::Processing);

        m.apply(Command::Inserted { latency_ms: 142 });
        assert_eq!(m.state(), OverlayState::Inserted);
        assert_eq!(m.latency_ms(), Some(142));

        // ... and takes itself off screen without being asked.
        run_to(
            &mut m,
            0,
            u64::from(INSERTED_HOLD_MS) + u64::from(EXIT_MS) + 40,
        );
        assert_eq!(m.state(), OverlayState::Hidden);
        assert!(m.is_idle(), "presence {}", m.presence());
    }

    #[test]
    fn inserted_holds_for_the_full_budget_before_leaving() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Inserted { latency_ms: 90 });

        run_to(&mut m, 0, u64::from(INSERTED_HOLD_MS) - 40);
        assert_eq!(m.state(), OverlayState::Inserted, "left too early");

        run_to(
            &mut m,
            u64::from(INSERTED_HOLD_MS) - 40,
            u64::from(INSERTED_HOLD_MS) + 20,
        );
        assert_eq!(m.state(), OverlayState::Hidden, "did not leave");
        // Still fading, so still drawing.
        assert!(!m.is_idle());
    }

    #[test]
    fn streaming_final_can_skip_processing() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Inserted { latency_ms: 60 });
        assert_eq!(m.state(), OverlayState::Inserted);
        assert_eq!(m.previous_state(), OverlayState::Listening);
    }

    #[test]
    fn stray_events_cannot_summon_a_dismissed_pill() {
        let mut m = model();
        m.apply(Command::Processing);
        assert_eq!(m.state(), OverlayState::Hidden);
        m.apply(Command::Inserted { latency_ms: 100 });
        assert_eq!(m.state(), OverlayState::Hidden);
        assert_eq!(m.latency_ms(), None);
        assert!(m.is_idle());
    }

    #[test]
    fn processing_is_only_reachable_from_listening() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Inserted { latency_ms: 1 });
        m.apply(Command::Processing);
        assert_eq!(
            m.state(),
            OverlayState::Inserted,
            "inserted must not rewind"
        );
    }

    #[test]
    fn re_triggering_while_inserted_starts_a_new_utterance() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Inserted { latency_ms: 142 });
        m.apply(Command::ShowListening);
        assert_eq!(m.state(), OverlayState::Listening);
        assert_eq!(m.latency_ms(), None, "stale latency carried over");
        assert_eq!(m.text(), "", "stale transcript carried over");
    }

    #[test]
    fn a_fresh_utterance_clears_the_previous_transcript() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::PartialText("the quarterly report".into()));
        m.apply(Command::Processing);
        m.apply(Command::Inserted { latency_ms: 90 });

        m.apply(Command::ShowListening);
        assert_eq!(m.text(), "");
    }

    #[test]
    fn show_listening_while_listening_does_not_restart_the_timer() {
        let mut m = model();
        m.apply(Command::ShowListening);
        run_to(&mut m, 0, 3_000);
        let elapsed = m.listening_ms();
        m.apply(Command::ShowListening);
        assert!(m.listening_ms() >= elapsed, "timer jumped backwards");
        assert!(m.listening_ms() >= 2_900);
    }

    #[test]
    fn hide_works_from_every_visible_state() {
        for setup in [
            vec![Command::ShowListening],
            vec![Command::ShowListening, Command::Processing],
            vec![
                Command::ShowListening,
                Command::Processing,
                Command::Inserted { latency_ms: 5 },
            ],
        ] {
            let mut m = model();
            for c in setup {
                m.apply(c);
            }
            m.apply(Command::Hide);
            assert_eq!(m.state(), OverlayState::Hidden);
            run_to(&mut m, 0, u64::from(EXIT_MS) + 40);
            assert!(m.is_idle());
        }
    }

    #[test]
    fn hide_while_hidden_is_a_no_op() {
        let mut m = model();
        m.apply(Command::Hide);
        assert_eq!(m.state(), OverlayState::Hidden);
        assert!(m.is_idle(), "a redundant hide restarted the exit animation");
    }

    #[test]
    fn shutdown_is_the_only_command_that_stops_the_loop() {
        let mut m = model();
        for c in [
            Command::ShowListening,
            Command::Level(0.5),
            Command::PartialText("hi".into()),
            Command::Engine("groq".into()),
            Command::Latched(true),
            Command::Processing,
            Command::Inserted { latency_ms: 1 },
            Command::Hide,
            Command::SetTheme(Box::new(PORCELAIN_LIGHT)),
        ] {
            assert!(m.apply(c), "command should not stop the loop");
        }
        assert!(!m.apply(Command::Shutdown));
    }

    #[test]
    fn latched_is_off_by_default_and_applies_regardless_of_state() {
        let mut m = model();
        assert!(!m.latched());

        // Applies even from `Hidden`, the same way `Level`/`PartialText` do —
        // it is a flag alongside the state machine, not a transition through
        // it.
        m.apply(Command::Latched(true));
        assert!(m.latched());

        m.apply(Command::ShowListening);
        m.apply(Command::Latched(true));
        assert!(m.latched());
        m.apply(Command::Latched(false));
        assert!(!m.latched());
    }

    /// A fresh utterance must never open already reading as latched, the same
    /// way it must never open holding the previous utterance's transcript or
    /// timer — see `Command::ShowListening`'s reset list. This is what stops a
    /// caller that forgot `Latched(false)` before its own `processing()` call
    /// from bleeding a stale latch into the very next dictation.
    #[test]
    fn a_fresh_show_listening_clears_a_stale_latch() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Latched(true));
        assert!(m.latched());

        m.apply(Command::Processing);
        m.apply(Command::Inserted { latency_ms: 90 });
        m.apply(Command::ShowListening);
        assert!(
            !m.latched(),
            "a latch from the previous utterance survived into this one"
        );
    }

    #[test]
    fn enter_completes_within_the_locked_budget() {
        let mut m = model();
        m.apply(Command::ShowListening);
        run_to(&mut m, 0, u64::from(ENTER_MS) + 20);
        assert!(
            (m.presence() - 1.0).abs() < 1e-3,
            "presence {}",
            m.presence()
        );
        // ... and is already most of the way there at a quarter of it, which is
        // the entire point of the front-loaded curve.
        let mut m = model();
        m.apply(Command::ShowListening);
        run_to(&mut m, 0, u64::from(ENTER_MS) / 4);
        assert!(m.presence() > 0.5, "presence {}", m.presence());
    }

    #[test]
    fn presence_never_leaves_the_unit_interval() {
        let mut m = model();
        let mut t = 0;
        for step in 0..400 {
            match step {
                20 => drop(m.apply(Command::ShowListening)),
                60 => drop(m.apply(Command::Processing)),
                80 => drop(m.apply(Command::Inserted { latency_ms: 120 })),
                200 => drop(m.apply(Command::ShowListening)),
                260 => drop(m.apply(Command::Hide)),
                _ => {}
            }
            t += 16;
            m.tick(t);
            assert!((0.0..=1.0).contains(&m.presence()), "step {step}");
        }
    }

    #[test]
    fn a_long_stall_does_not_teleport_the_animation() {
        let mut m = model();
        m.apply(Command::ShowListening);
        // The machine slept for a minute between frames.
        m.tick(60_000);
        assert!(m.presence() <= 1.0);
        assert!(m.level().is_finite());
    }

    #[test]
    fn the_timer_freezes_when_speech_ends() {
        let mut m = model();
        m.apply(Command::ShowListening);
        run_to(&mut m, 0, 3_400);
        m.apply(Command::Processing);
        let frozen = m.listening_ms();
        run_to(&mut m, 3_400, 6_000);
        assert_eq!(
            m.listening_ms(),
            frozen,
            "timer kept running while processing"
        );
    }

    #[test]
    fn timer_formats_like_a_stopwatch() {
        assert_eq!(format_timer(0), "0:00");
        assert_eq!(format_timer(999), "0:00");
        assert_eq!(format_timer(1_000), "0:01");
        assert_eq!(format_timer(63_000), "1:03");
        assert_eq!(format_timer(599_000), "9:59");
    }

    /// The overlay reserves the run's width away from the centred glyph's ink
    /// inside a capsule that is narrow by decision, so the readout growing a
    /// character would draw a digit straight through the spinner or the
    /// check. Every duration has to render at the width `format_timer(0)`
    /// does — including the ones nobody should ever produce.
    #[test]
    fn the_timer_readout_never_grows_past_four_characters() {
        let width = format_timer(0).chars().count();
        assert_eq!(width, 4);
        for ms in [0, 59_999, 599_999, 600_000, 3_600_000, 86_400_000, u64::MAX] {
            let shown = format_timer(ms);
            assert_eq!(
                shown.chars().count(),
                width,
                "{ms} ms formatted as {shown:?}, which is wider than the geometry reserves"
            );
        }
        assert_eq!(format_timer(600_000), "9:59", "the readout must saturate");
    }

    #[test]
    fn level_is_clamped_and_smoothed() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::Level(50.0));
        m.tick(16);
        assert!(m.level() <= 1.0, "{}", m.level());
        // Smoothed, not teleported.
        assert!(m.level() < 1.0);
        run_to(&mut m, 16, 400);
        assert!(m.level() > 0.9, "{}", m.level());

        m.apply(Command::Level(f32::NAN));
        run_to(&mut m, 400, 1_200);
        assert!(m.level().is_finite() && m.level() < 0.1, "{}", m.level());
    }

    #[test]
    fn level_rises_faster_than_it_falls() {
        let mut up = model();
        up.apply(Command::ShowListening);
        up.apply(Command::Level(1.0));
        run_to(&mut up, 0, 60);

        let mut down = model();
        down.apply(Command::ShowListening);
        down.apply(Command::Level(1.0));
        run_to(&mut down, 0, 500);
        let peak = down.level();
        down.apply(Command::Level(0.0));
        run_to(&mut down, 500, 560);

        assert!(up.level() > 0.7, "attack too slow: {}", up.level());
        assert!(
            down.level() > peak * 0.4,
            "release too fast: {}",
            down.level()
        );
    }

    #[test]
    fn text_updates_are_held_verbatim() {
        let mut m = model();
        m.apply(Command::ShowListening);
        m.apply(Command::PartialText("the quarterly".into()));
        assert_eq!(m.text(), "the quarterly");
        m.apply(Command::PartialText("the quarterly report".into()));
        assert_eq!(m.text(), "the quarterly report");
    }

    #[test]
    fn themes_switch_without_touching_anything_else() {
        let mut m = model();
        m.apply(Command::ShowListening);
        run_to(&mut m, 0, 500);
        let (state, presence) = (m.state(), m.presence());
        m.apply(Command::SetTheme(Box::new(PORCELAIN_LIGHT)));
        assert_eq!(m.theme().name, PORCELAIN_LIGHT.name);
        assert_eq!(m.state(), state);
        assert_eq!(m.presence(), presence);
    }

    #[test]
    fn cross_fade_progress_saturates() {
        let mut m = model();
        m.apply(Command::ShowListening);
        assert!(m.cross() < 0.1);
        run_to(&mut m, 0, u64::from(STATE_CROSS_MS) + 20);
        assert!((m.cross() - 1.0).abs() < 1e-3, "{}", m.cross());
    }

    #[test]
    fn the_pill_rises_and_grows_as_it_arrives() {
        let mut m = model();
        m.apply(Command::ShowListening);
        let (dy0, s0) = m.enter_transform(1.0);
        assert!((dy0 - 10.0).abs() < 0.01, "{dy0}");
        assert!((s0 - 0.90).abs() < 0.01, "{s0}");

        run_to(&mut m, 0, u64::from(ENTER_MS) + 20);
        let (dy1, s1) = m.enter_transform(1.0);
        assert!(dy1.abs() < 0.01, "{dy1}");
        assert!((s1 - 1.0).abs() < 0.01, "{s1}");

        // ... and scales the offset with DPI.
        let mut m = model();
        m.apply(Command::ShowListening);
        assert!((m.enter_transform(2.0).0 - 20.0).abs() < 0.02);
    }
}
