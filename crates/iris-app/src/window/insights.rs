//! Insights: statistics computed from the session log, and nothing else.
//!
//! Explicitly **not** the AI speech analyzer (`iris-ai-analyzer`, personality
//! / speaking-style / speech-rate) — that is a separate, later product. This
//! module only ever counts and averages what [`crate::history::DictationRecord`]
//! already records, off the dictation critical path (it runs on the window
//! thread, triggered by opening the window or its periodic refresh — see
//! `crate::window::state`). [`Insights::compute`] is pure, so a future,
//! heavier analyzer can sit beside it without this module changing shape.

use std::collections::HashMap;

use crate::history::DictationRecord;

/// How many entries [`Insights::top_words`] and [`Insights::top_phrases`] keep.
const TOP_N: usize = 8;

/// The stretch of UTC time one *local* calendar day covers, as the half-open
/// range `[start, end)`.
///
/// A range rather than a date prefix because the two calendars do not line
/// up: [`crate::history`] stamps every record in UTC (deliberately — reading
/// the local offset from a multi-threaded process is unsound, see that
/// module), while "Dictations today" is only meaningful on the user's own
/// clock. Converting the *day* once and comparing instants keeps the
/// conversion at the edge, so this module stays pure arithmetic; see
/// `crate::window::shell` for where the offset comes from.
///
/// Both bounds are `"YYYY-MM-DDTHH:MM:SS"` — RFC 3339 in UTC, truncated to
/// whole seconds, which is exactly the prefix [`DayWindow::contains`]
/// compares so a record's optional fractional seconds cannot skew it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DayWindow {
    start: String,
    end: String,
}

/// How many characters of an RFC 3339 UTC timestamp are compared: the
/// `"YYYY-MM-DDTHH:MM:SS"` prefix, which is fixed-width and therefore orders
/// lexicographically.
const SECOND_PRECISION: usize = 19;

impl DayWindow {
    /// A window from two UTC `"YYYY-MM-DDTHH:MM:SS"` bounds, `start`
    /// inclusive and `end` exclusive.
    #[must_use]
    pub fn new(start: impl Into<String>, end: impl Into<String>) -> Self {
        Self {
            start: start.into(),
            end: end.into(),
        }
    }

    /// Whether an RFC 3339 UTC `timestamp` falls inside this window. A
    /// timestamp too short to carry a whole second is never inside one.
    #[must_use]
    pub fn contains(&self, timestamp: &str) -> bool {
        let Some(at) = timestamp.get(..SECOND_PRECISION) else {
            return false;
        };
        at >= self.start.as_str() && at < self.end.as_str()
    }
}

/// A word or phrase and how many dictations it appeared in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// The word or phrase, lowercased.
    pub text: String,
    /// How many times it occurred.
    pub count: usize,
}

/// Statistics rolled up from the whole session log.
#[derive(Debug, Clone, PartialEq)]
pub struct Insights {
    /// Every entry in the log, successful or not.
    pub total_dictations: usize,
    /// Entries that fall on today's *local* calendar day — see [`DayWindow`]
    /// for why that is not the same as today's UTC date.
    pub dictations_today: usize,
    /// Words across every transcript (`text.split_whitespace().count()`).
    pub total_words: usize,
    /// Attempted dictations (something was said, or something went wrong)
    /// that reached the screen. See [`Insights::compute`] for what is
    /// excluded from the denominator and why.
    pub successes: usize,
    /// Attempted dictations that did not.
    pub failures: usize,
    /// Mean key-release-to-injected latency, milliseconds, over the dictations
    /// that reached the screen and carry a timing.
    ///
    /// A `perceived_ms` on its own is not that set: [`iris_core::latency::Timeline::perceived`]
    /// falls back to key-release → final transcript when nothing was injected,
    /// so a failed injection or a silent tap still carries a figure — a
    /// strictly shorter one, measuring a shorter span. Averaging the two
    /// together reads low exactly when injection is going wrong, so
    /// [`crate::history::DictationRecord::injected`] picks the input set.
    pub avg_latency_ms: Option<f64>,
    /// Median of the same set.
    pub median_latency_ms: Option<f64>,
    /// The most repeated words, stopwords and filler stripped, most first.
    pub top_words: Vec<Ranked>,
    /// The most repeated two- and three-word phrases, most first. Only
    /// phrases seen more than once — a phrase seen once is not "repeated".
    pub top_phrases: Vec<Ranked>,
}

impl Default for Insights {
    fn default() -> Self {
        Self::compute(&[], &DayWindow::default())
    }
}

impl Insights {
    /// Roll `records` up into [`Insights`].
    ///
    /// `today` is the caller's current *local* calendar day, expressed as the
    /// UTC range it covers — passed in rather than read from the clock so
    /// this stays a pure, deterministic function to test, and so the one
    /// place that has to ask the OS for a timezone stays in the native shell.
    ///
    /// # Success vs failure
    ///
    /// A pure silent hold (empty transcript, no error — the user tapped the
    /// key and said nothing) counts toward neither: it was not really a
    /// dictation *attempt*, and folding it into the denominator would make an
    /// accidental tap look identical to a failure. Everything else the user
    /// said, or that the engine tried and failed on, counts as an attempt;
    /// [`crate::history::DictationRecord::injected`] decides success.
    #[must_use]
    pub fn compute(records: &[DictationRecord], today: &DayWindow) -> Self {
        let total_dictations = records.len();
        let dictations_today = records
            .iter()
            .filter(|r| today.contains(&r.timestamp))
            .count();
        let total_words = records
            .iter()
            .map(|r| r.text.split_whitespace().count())
            .sum();

        let attempted = records
            .iter()
            .filter(|r| !r.text.is_empty() || r.error.is_some());
        let (mut successes, mut failures) = (0, 0);
        for record in attempted {
            if record.injected {
                successes += 1;
            } else {
                failures += 1;
            }
        }

        let mut latencies: Vec<f64> = records
            .iter()
            .filter(|r| r.injected)
            .filter_map(|r| r.latency.perceived_ms)
            .collect();
        latencies.sort_by(|a, b| a.total_cmp(b));
        let avg_latency_ms = mean(&latencies);
        let median_latency_ms = median(&latencies);

        let texts: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
        let top_words = top_words(&texts);
        let top_phrases = top_phrases(&texts);

        Self {
            total_dictations,
            dictations_today,
            total_words,
            successes,
            failures,
            avg_latency_ms,
            median_latency_ms,
            top_words,
            top_phrases,
        }
    }

    /// `successes / (successes + failures)`, `None` when nothing was attempted.
    #[must_use]
    pub fn success_rate(&self) -> Option<f64> {
        let attempted = self.successes + self.failures;
        (attempted > 0).then(|| self.successes as f64 / attempted as f64)
    }
}

fn mean(sorted: &[f64]) -> Option<f64> {
    (!sorted.is_empty()).then(|| sorted.iter().sum::<f64>() / sorted.len() as f64)
}

fn median(sorted: &[f64]) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// Lowercased, punctuation-stripped words, in order, empty tokens dropped.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

fn top_words(texts: &[&str]) -> Vec<Ranked> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts {
        for word in tokenize(text) {
            if word.chars().count() < 2 || is_stopword(&word) {
                continue;
            }
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    ranked(counts, 1)
}

/// Two- and three-word windows over each transcript's tokens, counted
/// together so a phrase repeated as both a bigram and part of a trigram still
/// shows up once each, ranked by raw occurrence.
fn top_phrases(texts: &[&str]) -> Vec<Ranked> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts {
        let words = tokenize(text);
        for window in [2usize, 3] {
            for slice in words.windows(window) {
                if slice.iter().all(|w| is_stopword(w)) {
                    continue;
                }
                *counts.entry(slice.join(" ")).or_insert(0) += 1;
            }
        }
    }
    ranked(counts, 2)
}

/// Sort by count desc, then text asc for a deterministic order, keep the top
/// [`TOP_N`] entries with at least `min_count` occurrences.
fn ranked(counts: HashMap<String, usize>, min_count: usize) -> Vec<Ranked> {
    let mut entries: Vec<Ranked> = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_count)
        .map(|(text, count)| Ranked { text, count })
        .collect();
    entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    entries.truncate(TOP_N);
    entries
}

/// Grammatical stopwords plus the fillers dictation actually produces
/// ("um", "uh", "like" as a hedge, "gonna"/"wanna" contractions). Not
/// exhaustive — the bar is "the headline insight is not 'the'", not a
/// linguistically complete stopword corpus.
fn is_stopword(word: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        "a",
        "an",
        "the",
        "and",
        "or",
        "but",
        "if",
        "so",
        "because",
        "as",
        "of",
        "in",
        "on",
        "at",
        "to",
        "for",
        "with",
        "from",
        "by",
        "about",
        "into",
        "over",
        "after",
        "before",
        "up",
        "down",
        "out",
        "off",
        "than",
        "then",
        "that",
        "this",
        "these",
        "those",
        "it",
        "its",
        "it's",
        "i",
        "i'm",
        "i'll",
        "i've",
        "i'd",
        "you",
        "you're",
        "you'll",
        "your",
        "yours",
        "he",
        "him",
        "his",
        "she",
        "her",
        "hers",
        "we",
        "we're",
        "our",
        "ours",
        "they",
        "them",
        "their",
        "is",
        "am",
        "are",
        "was",
        "were",
        "be",
        "been",
        "being",
        "have",
        "has",
        "had",
        "do",
        "does",
        "did",
        "will",
        "would",
        "should",
        "could",
        "can",
        "may",
        "might",
        "must",
        "not",
        "no",
        "yes",
        "just",
        "very",
        "really",
        "actually",
        "basically",
        "literally",
        "um",
        "umm",
        "uh",
        "uhh",
        "like",
        "okay",
        "ok",
        "well",
        "right",
        "gonna",
        "wanna",
        "kinda",
        "sorta",
        "there",
        "here",
        "what",
        "which",
        "who",
        "whom",
        "when",
        "where",
        "why",
        "how",
        "all",
        "some",
        "any",
        "each",
        "other",
        "such",
        "my",
        "me",
        "us",
        "also",
    ];
    STOPWORDS.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::LatencyBreakdown;

    fn record_at(
        text: &str,
        timestamp: &str,
        injected: bool,
        error: Option<&str>,
    ) -> DictationRecord {
        DictationRecord {
            timestamp: timestamp.to_string(),
            engine: "mock".to_string(),
            text: text.to_string(),
            raw: None,
            polish: None,
            injected,
            error: error.map(str::to_string),
            connection_failed: false,
            connection_cause: None,
            latency: LatencyBreakdown::default(),
        }
    }

    fn ok(text: &str, timestamp: &str) -> DictationRecord {
        record_at(text, timestamp, true, None)
    }

    /// 2026-08-01 for a user on UTC — the day every fixture below is stamped
    /// in. `local_day` is what builds the real one.
    fn today() -> DayWindow {
        DayWindow::new("2026-08-01T00:00:00", "2026-08-02T00:00:00")
    }

    #[test]
    fn empty_history_is_all_zeroes_and_none() {
        let insights = Insights::compute(&[], &today());
        assert_eq!(insights.total_dictations, 0);
        assert_eq!(insights.dictations_today, 0);
        assert_eq!(insights.total_words, 0);
        assert_eq!(insights.successes, 0);
        assert_eq!(insights.failures, 0);
        assert_eq!(insights.avg_latency_ms, None);
        assert_eq!(insights.median_latency_ms, None);
        assert_eq!(insights.success_rate(), None);
        assert!(insights.top_words.is_empty());
        assert!(insights.top_phrases.is_empty());
    }

    #[test]
    fn counts_words_and_todays_dictations() {
        let records = vec![
            ok("hello world", "2026-08-01T09:00:00Z"),
            ok("three word phrase", "2026-08-01T10:00:00Z"),
            ok("yesterday's words here", "2026-07-31T10:00:00Z"),
        ];
        let insights = Insights::compute(&records, &today());
        assert_eq!(insights.total_dictations, 3);
        assert_eq!(insights.dictations_today, 2);
        assert_eq!(insights.total_words, 2 + 3 + 3);
    }

    /// The whole point of the range: for a user on UTC-8, a dictation at
    /// 22:00 local is stamped in the *next* UTC day, and one made at 09:00
    /// UTC is still yesterday evening to them. A UTC-date prefix gets both
    /// backwards.
    #[test]
    fn a_local_day_is_not_the_utc_day() {
        // 2026-08-01 in UTC-8 runs 08:00Z on the 1st to 08:00Z on the 2nd.
        let local = DayWindow::new("2026-08-01T08:00:00", "2026-08-02T08:00:00");
        let records = vec![
            ok("last night, their time", "2026-08-01T05:00:00Z"),
            ok("this morning", "2026-08-01T17:00:00Z"),
            ok("tonight", "2026-08-02T06:00:00Z"),
            ok("tomorrow morning", "2026-08-02T17:00:00Z"),
        ];
        assert_eq!(Insights::compute(&records, &local).dictations_today, 2);
    }

    #[test]
    fn a_day_window_is_half_open_and_ignores_fractional_seconds() {
        let day = today();
        assert!(day.contains("2026-08-01T00:00:00Z"));
        assert!(day.contains("2026-08-01T00:00:00.482913Z"));
        assert!(day.contains("2026-08-01T23:59:59.999Z"));
        assert!(!day.contains("2026-08-02T00:00:00Z"));
        assert!(!day.contains("2026-07-31T23:59:59Z"));
        assert!(!day.contains("2026-08-01"), "not a whole second");
        assert!(!DayWindow::default().contains("2026-08-01T09:00:00Z"));
    }

    #[test]
    fn a_silent_tap_counts_toward_neither_success_nor_failure() {
        let silent = record_at("", "2026-08-01T09:00:00Z", false, None);
        let insights = Insights::compute(&[silent], &today());
        assert_eq!(insights.successes, 0);
        assert_eq!(insights.failures, 0);
        assert_eq!(insights.success_rate(), None);
    }

    #[test]
    fn an_engine_failure_with_no_text_still_counts_as_a_failure() {
        let failed = record_at("", "2026-08-01T09:00:00Z", false, Some("engine timed out"));
        let insights = Insights::compute(&[failed], &today());
        assert_eq!(insights.successes, 0);
        assert_eq!(insights.failures, 1);
        assert_eq!(insights.success_rate(), Some(0.0));
    }

    #[test]
    fn a_failed_injection_with_text_counts_as_a_failure_not_silence() {
        let failed = record_at(
            "the words that never made it",
            "2026-08-01T09:00:00Z",
            false,
            Some("injection failed: SendInput delivered 0 of 42 events"),
        );
        let insights = Insights::compute(&[failed], &today());
        assert_eq!(insights.failures, 1);
        // The words are still counted — this record is why the feature exists.
        assert_eq!(insights.total_words, 6);
    }

    #[test]
    fn success_rate_is_the_fraction_injected() {
        let records = vec![
            ok("one", "2026-08-01T09:00:00Z"),
            ok("two", "2026-08-01T09:00:00Z"),
            record_at("three", "2026-08-01T09:00:00Z", false, Some("boom")),
            record_at("", "2026-08-01T09:00:00Z", false, None), // excluded
        ];
        let insights = Insights::compute(&records, &today());
        assert_eq!(insights.successes, 2);
        assert_eq!(insights.failures, 1);
        assert!((insights.success_rate().unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn latency_mean_and_median_ignore_records_with_no_timing() {
        let mut a = ok("a", "2026-08-01T09:00:00Z");
        a.latency.perceived_ms = Some(100.0);
        let mut b = ok("b", "2026-08-01T09:00:00Z");
        b.latency.perceived_ms = Some(200.0);
        let mut c = ok("c", "2026-08-01T09:00:00Z");
        c.latency.perceived_ms = Some(300.0);
        let untimed = ok("d", "2026-08-01T09:00:00Z"); // perceived_ms: None

        let insights = Insights::compute(&[a, b, c, untimed], &today());
        assert_eq!(insights.avg_latency_ms, Some(200.0));
        assert_eq!(insights.median_latency_ms, Some(200.0));
    }

    /// A record that never reached the screen still carries a `perceived_ms`
    /// — the shorter key-release → final-transcript span, injection left out.
    /// Averaging it in with the injected ones pulls the tiles down precisely
    /// when delivery is failing, which is when they must be trusted.
    #[test]
    fn latency_is_measured_over_injected_dictations_only() {
        let mut injected = ok("landed", "2026-08-01T09:00:00Z");
        injected.latency.perceived_ms = Some(400.0);
        let mut failed = record_at(
            "never made it",
            "2026-08-01T09:00:00Z",
            false,
            Some("injection failed: SendInput delivered 0 of 42 events"),
        );
        failed.latency.perceived_ms = Some(100.0);
        let mut silent = record_at("", "2026-08-01T09:00:00Z", false, None);
        silent.latency.perceived_ms = Some(50.0);

        let insights = Insights::compute(&[injected, failed, silent], &today());
        assert_eq!(insights.avg_latency_ms, Some(400.0));
        assert_eq!(insights.median_latency_ms, Some(400.0));
    }

    /// And with nothing injected at all there is no measurement to report,
    /// rather than the shorter one dressed up as the perceived latency.
    #[test]
    fn latency_is_none_when_nothing_was_injected() {
        let mut failed = record_at("gone", "2026-08-01T09:00:00Z", false, Some("boom"));
        failed.latency.perceived_ms = Some(100.0);
        let insights = Insights::compute(&[failed], &today());
        assert_eq!(insights.avg_latency_ms, None);
        assert_eq!(insights.median_latency_ms, None);
    }

    #[test]
    fn median_of_an_even_count_averages_the_middle_two() {
        let mut a = ok("a", "2026-08-01T09:00:00Z");
        a.latency.perceived_ms = Some(100.0);
        let mut b = ok("b", "2026-08-01T09:00:00Z");
        b.latency.perceived_ms = Some(200.0);
        let insights = Insights::compute(&[a, b], &today());
        assert_eq!(insights.median_latency_ms, Some(150.0));
    }

    #[test]
    fn stopwords_and_filler_are_stripped_from_top_words() {
        let records = vec![
            ok("the the the and and of", "2026-08-01T09:00:00Z"),
            ok(
                "um so like the project is the project",
                "2026-08-01T09:00:00Z",
            ),
        ];
        let insights = Insights::compute(&records, &today());
        let words: Vec<&str> = insights.top_words.iter().map(|r| r.text.as_str()).collect();
        assert!(!words.contains(&"the"), "{words:?}");
        assert!(!words.contains(&"um"), "{words:?}");
        assert!(!words.contains(&"like"), "{words:?}");
        assert_eq!(insights.top_words[0].text, "project");
        assert_eq!(insights.top_words[0].count, 2);
    }

    #[test]
    fn a_word_said_only_once_still_appears() {
        let records = vec![ok("unique", "2026-08-01T09:00:00Z")];
        let insights = Insights::compute(&records, &today());
        assert_eq!(
            insights.top_words[0],
            Ranked {
                text: "unique".into(),
                count: 1
            }
        );
    }

    #[test]
    fn top_words_are_capped_and_ordered_by_count_then_alphabetically() {
        let text =
            "zebra zebra zebra yak yak xray xray wolf viper unicorn tiger snake rabbit quail";
        let insights = Insights::compute(&[ok(text, "2026-08-01T09:00:00Z")], &today());
        assert_eq!(insights.top_words.len(), TOP_N);
        assert_eq!(insights.top_words[0].text, "zebra");
        assert_eq!(insights.top_words[1].text, "xray"); // count 2, ties broken alphabetically before "yak"
        assert_eq!(insights.top_words[2].text, "yak");
    }

    #[test]
    fn a_phrase_repeated_twice_outranks_words_that_only_appear_once() {
        let records = vec![
            ok("thank you so much", "2026-08-01T09:00:00Z"),
            ok("thank you again", "2026-08-01T09:00:00Z"),
        ];
        let insights = Insights::compute(&records, &today());
        assert_eq!(insights.top_phrases[0].text, "thank you");
        assert_eq!(insights.top_phrases[0].count, 2);
    }

    #[test]
    fn a_phrase_seen_only_once_is_not_repeated_so_it_is_dropped() {
        let records = vec![ok("a completely unique sentence", "2026-08-01T09:00:00Z")];
        let insights = Insights::compute(&records, &today());
        assert!(
            insights.top_phrases.is_empty(),
            "{:?}",
            insights.top_phrases
        );
    }

    #[test]
    fn a_phrase_made_entirely_of_stopwords_is_dropped() {
        let records = vec![
            ok("this is of the it", "2026-08-01T09:00:00Z"),
            ok("of the it is this", "2026-08-01T09:00:00Z"),
        ];
        let insights = Insights::compute(&records, &today());
        for phrase in &insights.top_phrases {
            let all_stopwords = phrase.text.split(' ').all(is_stopword);
            assert!(!all_stopwords, "{phrase:?} should have been dropped");
        }
    }

    #[test]
    fn tokenize_keeps_apostrophes_inside_words_and_splits_on_punctuation() {
        assert_eq!(
            tokenize("Don't stop, please—stop!"),
            vec!["don't", "stop", "please", "stop"]
        );
    }
}
