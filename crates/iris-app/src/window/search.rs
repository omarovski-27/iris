//! The History tab's search box: a plain case-insensitive substring match.
//!
//! Pure and separate from `crate::window::ui` so it is unit-testable without
//! touching `egui` at all.

use crate::history::DictationRecord;

/// The positions in `records` (already newest-first) that match `query`.
///
/// An empty query matches everything. Matching is on the injected text and
/// the engine name — a search for "deepgram" is a reasonable thing to type
/// when hunting for "that one dictation from the streaming engine".
///
/// Indices rather than references so the result can be cached beside the
/// records themselves — see [`crate::window::WindowState::sync_filter`],
/// which is what keeps this scan off the per-frame path.
#[must_use]
pub fn filter_indices(records: &[DictationRecord], query: &str) -> Vec<usize> {
    // Trimmed and lowercased once for the whole scan, not once per record.
    let query = query.trim().to_lowercase();
    records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches(r, &query))
        .map(|(i, _)| i)
        .collect()
}

/// Whether `record` should show for an already-normalized `query`.
fn matches(record: &DictationRecord, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    record.text.to_lowercase().contains(query) || record.engine.to_lowercase().contains(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(text: &str, engine: &str) -> DictationRecord {
        let mut r = DictationRecord::now(engine, text);
        r.injected = true;
        r
    }

    /// Whether the one record in `records` survives `query`.
    fn hit(records: &[DictationRecord], query: &str) -> bool {
        !filter_indices(records, query).is_empty()
    }

    #[test]
    fn empty_query_matches_everything() {
        let records = vec![record("hello world", "mock")];
        assert!(hit(&records, ""));
        assert!(hit(&records, "   "));
    }

    #[test]
    fn matches_text_case_insensitively() {
        let records = vec![record("Hello World", "mock")];
        assert!(hit(&records, "world"));
        assert!(hit(&records, "HELLO"));
        assert!(!hit(&records, "goodbye"));
    }

    #[test]
    fn matches_the_engine_name_too() {
        let records = vec![record("some text", "deepgram")];
        assert!(hit(&records, "deep"));
        assert!(!hit(&records, "groq"));
    }

    #[test]
    fn filter_indices_preserves_order_and_drops_non_matches() {
        let records = vec![
            record("apples and oranges", "mock"),
            record("just bananas", "mock"),
            record("more apples", "groq"),
        ];
        assert_eq!(filter_indices(&records, "apple"), vec![0, 2]);
    }

    #[test]
    fn an_empty_query_keeps_every_index() {
        let records = vec![record("one", "mock"), record("two", "mock")];
        assert_eq!(filter_indices(&records, "  "), vec![0, 1]);
    }
}
