//! The session log: one JSON object per dictation, newest last.
//!
//! # Why a log at all
//!
//! Injection can fail — the focused window is elevated, the app swallowed the
//! keystrokes, the user alt-tabbed mid-dictation. When it does, the words the
//! user just spoke are otherwise gone, and there is nothing more infuriating in
//! a dictation tool. So **every** dictation is recorded, successful or not,
//! with the text and the reason, and the file is the recovery path.
//!
//! It is also the seed of the history feature: a jsonl file of
//! `{timestamp, engine, text, latency}` is exactly what a "recent dictations"
//! window needs, and appending one line costs microseconds off the critical
//! path (the append happens after the text has been injected, never before).
//!
//! # Format
//!
//! JSON Lines: one [`DictationRecord`] per line, so a partially written last
//! line loses one entry rather than the file. The file is trimmed to the newest
//! [`SessionLog::max_entries`] entries as it grows, because this is a resident
//! app that may run for months.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use iris_core::latency::{ms, Mark, Timeline};
use serde::{Deserialize, Serialize};

/// The latency breakdown for one dictation, in milliseconds.
///
/// Mirrors [`iris_core::latency::Timeline`], which holds [`std::time::Instant`]s
/// and therefore cannot be serialised: an `Instant` has no meaning in another
/// process, let alone another boot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyBreakdown {
    /// Key-press → the engine session existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_open_ms: Option<f64>,
    /// Key-press → the first audio frame arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_start_ms: Option<f64>,
    /// Key-press → the transport was ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_ready_ms: Option<f64>,
    /// First audio → first interim transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_partial_ms: Option<f64>,
    /// Key-release → final transcript. The dominant term the user feels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_transcript_ms: Option<f64>,
    /// Time spent in the polisher.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polish_ms: Option<f64>,
    /// Final transcript → text on screen. Includes polish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject_ms: Option<f64>,
    /// Key-release → text on screen. The number the user actually feels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perceived_ms: Option<f64>,
    /// Seconds of audio captured.
    pub audio_secs: f64,
    /// How many interim transcripts arrived.
    pub partials: usize,
}

impl LatencyBreakdown {
    /// Project a [`Timeline`] onto the serialisable spans.
    pub fn from_timeline(timeline: &Timeline) -> Self {
        let span = |from, to| timeline.delta(from, to).map(ms);
        Self {
            session_open_ms: span(Mark::KeyDown, Mark::SessionOpen),
            capture_start_ms: span(Mark::KeyDown, Mark::CaptureStart),
            stream_ready_ms: span(Mark::KeyDown, Mark::StreamReady),
            first_partial_ms: span(Mark::CaptureStart, Mark::FirstPartial),
            final_transcript_ms: span(Mark::KeyUp, Mark::FinalTranscript),
            polish_ms: None,
            inject_ms: span(Mark::FinalTranscript, Mark::Injected),
            perceived_ms: timeline.perceived().map(ms),
            audio_secs: timeline.audio_secs,
            partials: timeline.partials,
        }
    }
}

/// How the transcript was cleaned up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolishInfo {
    /// Which polisher produced the text: `llm`, `rule`, `mock`, `unchanged`.
    pub source: String,
    /// Set when the deadline or an error forced the fallback path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// One dictation, as it happened.
///
/// Note what is *not* here: no API keys, no audio, no device identifiers. The
/// file is plain text on the user's disk and should stay boring enough to paste
/// into a bug report after a glance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DictationRecord {
    /// RFC 3339, UTC. Wall clock, because a monotonic instant means nothing to
    /// the person reading the file.
    pub timestamp: String,
    /// Which engine produced the transcript.
    pub engine: String,
    /// The text as injected — polished, without the trailing space.
    pub text: String,
    /// The engine's transcript before polish. Omitted when polish changed
    /// nothing, which is the common case for the rule engine on clean speech.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// How the text was cleaned up, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polish: Option<PolishInfo>,
    /// Whether the text reached the focused window.
    pub injected: bool,
    /// Why the dictation did not end in injected text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set when this dictation never reached a real connection to the
    /// transcription service — no internet, DNS down, the service
    /// unreachable, or (rarer) a rejected key, all of which look the same
    /// from here: the engine never got as far as a socket. Distinguishing
    /// this from an ordinary capture failure matters for diagnosis — see
    /// `AGENTS.md`'s "almost no audio reached the transcription engine" entry
    /// on how much a prose-only distinction cost a previous investigation.
    /// `#[serde(default)]` so a session log written before this field existed
    /// still parses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub connection_failed: bool,
    /// The specific, classified reason this dictation could not reach or use
    /// the transcription service — a short, stable, greppable label
    /// (`"invalid_key"`, `"exhausted_credit"`, `"rate_limited"`,
    /// `"network_unreachable"`, `"timeout"`; see
    /// `iris_core::engine::FailureCause::label`) — separate from
    /// `connection_failed`: that field is about whether a real network
    /// connection was ever reached (see its own doc comment), whereas this
    /// one is set whenever the provider itself named a specific cause,
    /// whether or not the engine's own connect bookkeeping considers this a
    /// "never connected" case. Groq, which never sets `connection_failed`
    /// (it has no real connect step to measure), can still set this field.
    /// `None` for an unclassified failure or an ordinary successful
    /// dictation — never a guess. `#[serde(default)]` so a session log
    /// written before this field existed still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_cause: Option<String>,
    /// The latency breakdown.
    pub latency: LatencyBreakdown,
}

impl DictationRecord {
    /// A record stamped with the current wall clock.
    pub fn now(engine: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            timestamp: timestamp(),
            engine: engine.into(),
            text: text.into(),
            raw: None,
            polish: None,
            injected: false,
            error: None,
            connection_failed: false,
            connection_cause: None,
            latency: LatencyBreakdown::default(),
        }
    }
}

/// The current time, RFC 3339 in UTC.
///
/// UTC rather than local time on purpose: `time`'s local offset lookup is
/// documented as unsound in a multi-threaded process, and Iris is very much
/// multi-threaded by the time it writes its first record.
fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        // The formatter can only fail on a value it cannot represent, which
        // `now_utc` never is; a log line is not worth propagating an error for.
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

/// An append-only, size-capped jsonl file.
///
/// `Clone` exists for [`crate::app::App`]'s quit-during-finalize path: a
/// dictation whose finalise is still running when the tray quits is finished
/// on a detached thread (see `App::capture`'s doc comment), which needs its
/// own handle to append the eventual record. The clone's `entries` count can
/// drift from the original's if both append around the same moment, which
/// only blurs *when* the next trim happens, not correctness.
#[derive(Debug, Clone)]
pub struct SessionLog {
    path: PathBuf,
    max_entries: usize,
    enabled: bool,
    /// Entries currently in the file. Counted once on open so the common path
    /// is an append with no read.
    entries: usize,
}

impl SessionLog {
    /// Open (without creating) the log at `path`, keeping at most
    /// `max_entries`.
    pub fn open(path: impl Into<PathBuf>, max_entries: usize) -> Self {
        let path = path.into();
        let entries = count_lines(&path).unwrap_or(0);
        Self {
            path,
            max_entries: max_entries.max(1),
            enabled: true,
            entries,
        }
    }

    /// A log that drops everything, for `history.enabled = false`.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            max_entries: 1,
            enabled: false,
            entries: 0,
        }
    }

    /// Where the log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether records are actually written. A disabled log has no path worth
    /// showing to the user.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// How many entries the file is allowed to hold.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Append one record, trimming the file if it has outgrown its cap.
    ///
    /// Called after injection, so the cost is off the latency path the user
    /// feels — but it is still one `write` of a few hundred bytes in the common
    /// case, because the trim only runs when the cap is actually exceeded.
    pub fn append(&mut self, record: &DictationRecord) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let line = serde_json::to_string(record).context("encoding a session-log entry")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(file, "{line}").with_context(|| format!("writing {}", self.path.display()))?;
        self.entries += 1;

        if self.entries > self.max_entries {
            self.trim()?;
        }
        Ok(())
    }

    /// Rewrite the file with only the newest `max_entries` lines.
    fn trim(&mut self) -> Result<()> {
        let kept: Vec<String> = read_lines(&self.path)?
            .into_iter()
            .rev()
            .take(self.max_entries)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        // Write-and-rename: a crash mid-trim must not be able to lose the whole
        // history, which is the one thing this file exists to protect.
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, kept.join("\n") + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        self.entries = kept.len();
        Ok(())
    }

    /// Read every record back, oldest first.
    ///
    /// Unparseable lines are skipped rather than failing the read: a truncated
    /// last line from a power cut should cost one entry, not the history.
    pub fn read_all(path: &Path) -> Result<Vec<DictationRecord>> {
        Ok(read_lines(path)?
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let lines = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect();
    Ok(lines)
}

fn count_lines(path: &Path) -> Result<usize> {
    Ok(read_lines(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(text: &str) -> DictationRecord {
        DictationRecord::now("mock", text)
    }

    #[test]
    fn appends_and_reads_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("history.jsonl");
        let mut log = SessionLog::open(&path, 10);

        log.append(&record("first")).unwrap();
        log.append(&record("second")).unwrap();

        let all = SessionLog::read_all(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "first");
        assert_eq!(all[1].text, "second");
        assert!(all[0].timestamp.contains('T'), "{}", all[0].timestamp);
    }

    #[test]
    fn trims_to_the_newest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut log = SessionLog::open(&path, 3);

        for i in 0..10 {
            log.append(&record(&format!("line {i}"))).unwrap();
        }

        let all = SessionLog::read_all(&path).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text, "line 7");
        assert_eq!(all[2].text, "line 9");
        assert!(!path.with_extension("jsonl.tmp").exists());
    }

    #[test]
    fn reopening_counts_what_is_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");

        let mut log = SessionLog::open(&path, 2);
        log.append(&record("a")).unwrap();
        log.append(&record("b")).unwrap();
        drop(log);

        // A fresh process must not have to re-read the whole file to know it is
        // already at the cap.
        let mut log = SessionLog::open(&path, 2);
        log.append(&record("c")).unwrap();
        let all = SessionLog::read_all(&path).unwrap();
        assert_eq!(
            all.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn a_failed_dictation_is_still_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut log = SessionLog::open(&path, 10);

        let mut rec = record("the words the user lost");
        rec.error = Some("SendInput delivered 0 of 42 events".into());
        log.append(&rec).unwrap();

        let all = SessionLog::read_all(&path).unwrap();
        assert!(!all[0].injected);
        assert_eq!(all[0].text, "the words the user lost");
        assert!(all[0].error.as_deref().unwrap().contains("SendInput"));
    }

    #[test]
    fn a_truncated_line_costs_one_entry_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut log = SessionLog::open(&path, 10);
        log.append(&record("intact")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"timestamp\":\"2026-01-01T00:0")
            .unwrap();

        let all = SessionLog::read_all(&path).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, "intact");
    }

    #[test]
    fn a_disabled_log_writes_nothing() {
        let mut log = SessionLog::disabled();
        log.append(&record("nothing to see")).unwrap();
        assert_eq!(log.path(), Path::new(""));
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SessionLog::read_all(&dir.path().join("absent.jsonl"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn latency_projects_the_spans_it_has() {
        use std::time::{Duration, Instant};

        let origin = Instant::now();
        let mut timeline = Timeline::start_at("mock", origin);
        timeline.mark_at(Mark::SessionOpen, origin + Duration::from_millis(1));
        timeline.mark_at(Mark::KeyUp, origin + Duration::from_millis(2_000));
        timeline.mark_at(Mark::FinalTranscript, origin + Duration::from_millis(2_100));
        timeline.mark_at(Mark::Injected, origin + Duration::from_millis(2_160));
        timeline.audio_secs = 2.0;
        timeline.partials = 4;

        let latency = LatencyBreakdown::from_timeline(&timeline);
        assert_eq!(latency.session_open_ms, Some(1.0));
        assert_eq!(latency.final_transcript_ms, Some(100.0));
        assert_eq!(latency.inject_ms, Some(60.0));
        assert_eq!(latency.perceived_ms, Some(160.0));
        assert_eq!(latency.partials, 4);
        // Never stamped, so absent rather than zero — zero would be a lie.
        assert_eq!(latency.first_partial_ms, None);
        assert_eq!(latency.capture_start_ms, None);
    }

    #[test]
    fn records_survive_a_json_round_trip() {
        let mut rec = record("hello");
        rec.raw = Some("um hello".into());
        rec.polish = Some(PolishInfo {
            source: "llm".into(),
            fallback: None,
        });
        rec.injected = true;
        rec.latency.polish_ms = Some(12.5);

        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<DictationRecord>(&json).unwrap(), rec);
    }
}
