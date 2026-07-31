//! Latency instrumentation.
//!
//! Every dictation carries a [`Timeline`]: a set of named [`Mark`]s stamped
//! with a monotonic [`Instant`]. The live pipeline and the offline harness stamp
//! the same marks, so a harness number and a real number are comparable.
//!
//! The one that matters is **perceived latency**: key-release → text on screen.
//! Everything before the key release happens while the user is still talking and
//! is therefore free, which is the entire reason for streaming while speaking.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// A point in the dictation pipeline worth timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mark {
    /// Hotkey pressed. The origin of every reported number.
    KeyDown,
    /// Engine session object created (before the transport is up).
    SessionOpen,
    /// The audio device delivered its first callback.
    CaptureStart,
    /// Transport ready to accept audio (websocket open, or immediately for
    /// engines that have no transport).
    StreamReady,
    /// First interim transcript came back from the engine.
    FirstPartial,
    /// Hotkey released. Speech is over; the user starts waiting *here*.
    KeyUp,
    /// Engine delivered the final transcript.
    FinalTranscript,
    /// Text handed to the OS for the focused window.
    Injected,
}

impl Mark {
    pub fn label(self) -> &'static str {
        match self {
            Mark::KeyDown => "key-down",
            Mark::SessionOpen => "session-open",
            Mark::CaptureStart => "capture-start",
            Mark::StreamReady => "stream-ready",
            Mark::FirstPartial => "first-partial",
            Mark::KeyUp => "key-up",
            Mark::FinalTranscript => "final-transcript",
            Mark::Injected => "injected",
        }
    }
}

/// Ordered record of when each [`Mark`] happened.
#[derive(Debug, Clone)]
pub struct Timeline {
    origin: Instant,
    marks: Vec<(Mark, Instant)>,
    /// Seconds of audio fed to the engine.
    pub audio_secs: f64,
    /// How many interim transcripts arrived.
    pub partials: usize,
    pub engine: String,
    pub transcript: String,
}

impl Timeline {
    /// Start a timeline whose origin is now.
    pub fn start(engine: impl Into<String>) -> Self {
        Self::start_at(engine, Instant::now())
    }

    /// Start a timeline whose origin is when the key actually went down.
    ///
    /// The hook stamps the key press before it reaches us, and every reported
    /// number is relative to that instant, so the few hundred microseconds of
    /// channel hop are attributed honestly rather than hidden.
    pub fn start_at(engine: impl Into<String>, origin: Instant) -> Self {
        let mut t = Self {
            origin,
            marks: Vec::with_capacity(8),
            audio_secs: 0.0,
            partials: 0,
            engine: engine.into(),
            transcript: String::new(),
        };
        t.marks.push((Mark::KeyDown, origin));
        t
    }

    /// Stamp `mark` with the current instant. The first stamp of a mark wins,
    /// which is what we want for `FirstPartial`.
    pub fn mark(&mut self, mark: Mark) {
        self.mark_at(mark, Instant::now());
    }

    pub fn mark_at(&mut self, mark: Mark, at: Instant) {
        if self.at(mark).is_none() {
            self.marks.push((mark, at));
        }
    }

    pub fn at(&self, mark: Mark) -> Option<Instant> {
        self.marks.iter().find(|(m, _)| *m == mark).map(|(_, i)| *i)
    }

    /// Elapsed time between two marks, if both were stamped.
    pub fn delta(&self, from: Mark, to: Mark) -> Option<Duration> {
        let a = self.at(from)?;
        let b = self.at(to)?;
        b.checked_duration_since(a)
    }

    pub fn since_start(&self, mark: Mark) -> Option<Duration> {
        self.at(mark).map(|i| i.duration_since(self.origin))
    }

    /// Key-release → text injected. The number the user actually feels.
    ///
    /// Falls back to key-release → final transcript when nothing was injected
    /// (the harness runs headless), so the harness still reports the dominant
    /// term.
    pub fn perceived(&self) -> Option<Duration> {
        self.delta(Mark::KeyUp, Mark::Injected)
            .or_else(|| self.delta(Mark::KeyUp, Mark::FinalTranscript))
    }

    /// True when [`Self::perceived`] includes real injection rather than
    /// stopping at the final transcript.
    pub fn perceived_includes_injection(&self) -> bool {
        self.at(Mark::Injected).is_some()
    }

    /// Human-readable per-dictation breakdown.
    pub fn report(&self, index: usize) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "\n── dictation #{index} ──────────────────────────────");
        let _ = writeln!(s, "  engine      {}", self.engine);
        let _ = writeln!(s, "  audio       {:.2} s", self.audio_secs);
        let _ = writeln!(s, "  partials    {}", self.partials);
        let _ = writeln!(s, "  transcript  {:?}", self.transcript);
        let _ = writeln!(s);

        for (from, to, what) in SPANS {
            if let Some(d) = self.delta(*from, *to) {
                let _ = writeln!(s, "  {:<34}{:>9.2} ms", what, ms(d));
            }
        }

        let _ = writeln!(s, "  {:-<69}", "");
        match self.perceived() {
            Some(d) => {
                let note = if self.perceived_includes_injection() {
                    "key-release → text on screen"
                } else {
                    "key-release → final transcript (no injection)"
                };
                let _ = writeln!(s, "  PERCEIVED {:<47}{:>9.2} ms", note, ms(d));
            }
            None => {
                let _ = writeln!(s, "  PERCEIVED  (incomplete timeline)");
            }
        }
        s
    }
}

/// The spans worth printing, in pipeline order.
const SPANS: &[(Mark, Mark, &str)] = &[
    (Mark::KeyDown, Mark::SessionOpen, "key-press → session open"),
    (
        Mark::KeyDown,
        Mark::CaptureStart,
        "key-press → first audio in",
    ),
    (Mark::KeyDown, Mark::StreamReady, "key-press → stream ready"),
    (
        Mark::CaptureStart,
        Mark::FirstPartial,
        "first audio → first partial",
    ),
    (
        Mark::KeyUp,
        Mark::FinalTranscript,
        "key-release → final transcript",
    ),
    (
        Mark::FinalTranscript,
        Mark::Injected,
        "final transcript → injected",
    ),
];

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

/// Aggregate of repeated harness runs.
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
}

impl Stats {
    /// `values` are milliseconds. Returns `None` for an empty sample.
    pub fn from_ms(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut v = values.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pick = |q: f64| {
            let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
            v[idx]
        };
        Some(Stats {
            n: v.len(),
            min: v[0],
            p50: pick(0.5),
            p95: pick(0.95),
            max: v[v.len() - 1],
            mean: v.iter().sum::<f64>() / v.len() as f64,
        })
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} min={:.2} p50={:.2} p95={:.2} max={:.2} mean={:.2} (ms)",
            self.n, self.min, self.p50, self.p95, self.max, self.mean
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_stamp_of_a_mark_wins() {
        let mut t = Timeline::start("mock");
        let a = Instant::now();
        let b = a + Duration::from_millis(50);
        t.mark_at(Mark::FirstPartial, a);
        t.mark_at(Mark::FirstPartial, b);
        assert_eq!(t.at(Mark::FirstPartial), Some(a));
    }

    #[test]
    fn perceived_prefers_injection_over_final() {
        let mut t = Timeline::start("mock");
        let up = Instant::now();
        t.mark_at(Mark::KeyUp, up);
        t.mark_at(Mark::FinalTranscript, up + Duration::from_millis(100));
        assert_eq!(t.perceived(), Some(Duration::from_millis(100)));
        assert!(!t.perceived_includes_injection());

        t.mark_at(Mark::Injected, up + Duration::from_millis(103));
        assert_eq!(t.perceived(), Some(Duration::from_millis(103)));
        assert!(t.perceived_includes_injection());
    }

    #[test]
    fn missing_marks_do_not_panic() {
        let t = Timeline::start("mock");
        assert!(t.delta(Mark::KeyUp, Mark::Injected).is_none());
        assert!(t.perceived().is_none());
        assert!(t.report(1).contains("incomplete timeline"));
    }

    #[test]
    fn stats_summarise_a_sample() {
        let s = Stats::from_ms(&[10.0, 20.0, 30.0, 40.0]).unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(s.min, 10.0);
        assert_eq!(s.max, 40.0);
        assert_eq!(s.mean, 25.0);
        assert!(Stats::from_ms(&[]).is_none());
    }
}
