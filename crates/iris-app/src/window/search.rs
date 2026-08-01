//! The History tab's search box: a plain case-insensitive substring match.
//!
//! Pure and separate from `crate::window::ui` so it is unit-testable without
//! touching `egui` at all.

use crate::history::DictationRecord;

/// Whether `record` should show for `query`.
///
/// An empty query matches everything. Matches on the injected text and the
/// engine name — a search for "deepgram" is a reasonable thing to type when
/// hunting for "that one dictation from the streaming engine".
#[must_use]
pub fn matches(record: &DictationRecord, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    record.text.to_lowercase().contains(&query) || record.engine.to_lowercase().contains(&query)
}

/// Filter `records` (already newest-first) down to the ones matching `query`.
#[must_use]
pub fn filter<'a>(records: &'a [DictationRecord], query: &str) -> Vec<&'a DictationRecord> {
    records.iter().filter(|r| matches(r, query)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(text: &str, engine: &str) -> DictationRecord {
        let mut r = DictationRecord::now(engine, text);
        r.injected = true;
        r
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(matches(&record("hello world", "mock"), ""));
        assert!(matches(&record("hello world", "mock"), "   "));
    }

    #[test]
    fn matches_text_case_insensitively() {
        assert!(matches(&record("Hello World", "mock"), "world"));
        assert!(matches(&record("Hello World", "mock"), "HELLO"));
        assert!(!matches(&record("Hello World", "mock"), "goodbye"));
    }

    #[test]
    fn matches_the_engine_name_too() {
        assert!(matches(&record("some text", "deepgram"), "deep"));
        assert!(!matches(&record("some text", "deepgram"), "groq"));
    }

    #[test]
    fn filter_preserves_order_and_drops_non_matches() {
        let records = vec![
            record("apples and oranges", "mock"),
            record("just bananas", "mock"),
            record("more apples", "groq"),
        ];
        let hits = filter(&records, "apple");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "apples and oranges");
        assert_eq!(hits[1].text, "more apples");
    }
}
