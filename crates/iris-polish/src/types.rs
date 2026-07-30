//! The data that flows in and out of a [`Polisher`](crate::Polisher).

use std::fmt;
use std::time::Duration;

/// A raw transcript plus whatever the caller knows about where the text is going.
///
/// The hints are advisory. A polisher that ignores them entirely (like
/// [`RulePolisher`](crate::RulePolisher)) is still correct.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PolishRequest {
    /// The raw speech-to-text output, exactly as the engine produced it.
    pub text: String,
    /// What the caller knows about the destination.
    pub hints: ContextHints,
}

impl PolishRequest {
    /// A request with no context hints.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            hints: ContextHints::default(),
        }
    }

    /// Attach context hints.
    #[must_use]
    pub fn with_hints(mut self, hints: ContextHints) -> Self {
        self.hints = hints;
        self
    }
}

impl From<&str> for PolishRequest {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for PolishRequest {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

/// What the caller knows about the text's destination.
///
/// Everything is optional and additive: this type is `#[non_exhaustive]` so new
/// hints (tone, prior text in the field, selected text being replaced, ...) can
/// land without breaking callers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContextHints {
    /// The application receiving the text, e.g. `"Slack"` or `"Visual Studio Code"`.
    pub target_app: Option<String>,
    /// The register the output should keep.
    pub style: Option<TextStyle>,
    /// Domain terms, names, and identifiers that must survive verbatim.
    ///
    /// This is the single highest-value hint for transcript cleanup: speech-to-text
    /// mangles project-specific vocabulary more than anything else, and a polisher
    /// that does not know the term is real will "correct" it into something wrong.
    pub vocabulary: Vec<String>,
    /// BCP-47 language tag of the speech, e.g. `"en-GB"`. Drives spelling conventions.
    pub locale: Option<String>,
}

impl ContextHints {
    /// Empty hints.
    pub fn new() -> Self {
        Self::default()
    }

    /// Name the destination application.
    #[must_use]
    pub fn with_target_app(mut self, app: impl Into<String>) -> Self {
        self.target_app = Some(app.into());
        self
    }

    /// Set the register of the output.
    #[must_use]
    pub fn with_style(mut self, style: TextStyle) -> Self {
        self.style = Some(style);
        self
    }

    /// Add terms that must be preserved verbatim.
    #[must_use]
    pub fn with_vocabulary<I, S>(mut self, terms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.vocabulary.extend(terms.into_iter().map(Into::into));
        self
    }

    /// Set the BCP-47 locale of the speech.
    #[must_use]
    pub fn with_locale(mut self, locale: impl Into<String>) -> Self {
        self.locale = Some(locale.into());
        self
    }

    /// True when no hint carries information.
    pub fn is_empty(&self) -> bool {
        self.target_app.is_none()
            && self.style.is_none()
            && self.vocabulary.is_empty()
            && self.locale.is_none()
    }
}

/// The register the polished text should keep.
///
/// This never licenses a rewrite; it only tells a polisher which conventions to
/// apply when it is already making a punctuation or casing decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextStyle {
    /// Documents, email, notes: full sentences and terminal punctuation.
    Prose,
    /// Chat and comment boxes: sentence case, but a missing final period is fine.
    Message,
    /// Editors and terminals: identifiers, paths, and symbols are load-bearing.
    Technical,
}

impl TextStyle {
    /// A one-line description for an LLM prompt.
    pub fn prompt_hint(self) -> &'static str {
        match self {
            Self::Prose => {
                "Destination is prose (document, email, notes): write full sentences \
                 with terminal punctuation."
            }
            Self::Message => {
                "Destination is a chat message: keep it conversational; a missing \
                 final period is acceptable; do not add formality."
            }
            Self::Technical => {
                "Destination is an editor or terminal: identifiers, paths, flags, and \
                 symbols are load-bearing. Never reformat or re-case them."
            }
        }
    }
}

impl fmt::Display for TextStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Prose => "prose",
            Self::Message => "message",
            Self::Technical => "technical",
        };
        f.write_str(s)
    }
}

/// Which polisher actually produced the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PolishSource {
    /// The deterministic rule engine.
    Rule,
    /// A language model.
    Llm,
    /// A test double.
    Mock,
    /// Returned verbatim: the polisher declined to change anything.
    Unchanged,
}

impl fmt::Display for PolishSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Rule => "rule",
            Self::Llm => "llm",
            Self::Mock => "mock",
            Self::Unchanged => "unchanged",
        };
        f.write_str(s)
    }
}

/// Why a [`FallbackPolisher`](crate::FallbackPolisher) used its fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FallbackReason {
    /// The primary polisher did not finish inside the latency budget.
    BudgetExceeded {
        /// The budget it blew, in milliseconds.
        budget_ms: u64,
    },
    /// The primary polisher returned an error.
    PolisherFailed(String),
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BudgetExceeded { budget_ms } => {
                write!(f, "primary polisher exceeded the {budget_ms} ms budget")
            }
            Self::PolisherFailed(why) => write!(f, "primary polisher failed: {why}"),
        }
    }
}

/// Cleaned text, plus how it got that way and what it cost.
///
/// The timing is not decoration: this step sits on the keystroke-to-text path, so
/// callers are expected to watch [`Polished::duration`] and degrade when it drifts.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Polished {
    /// The text to insert.
    pub text: String,
    /// Which implementation produced it.
    pub source: PolishSource,
    /// Wall-clock time spent inside `polish`.
    pub duration: Duration,
    /// Set when the result came from a fallback rather than the intended path.
    pub fallback: Option<FallbackReason>,
}

impl Polished {
    /// A successful result from `source`, taking `duration`.
    pub fn new(text: impl Into<String>, source: PolishSource, duration: Duration) -> Self {
        Self {
            text: text.into(),
            source,
            duration,
            fallback: None,
        }
    }

    /// Record that this result came from a fallback path.
    #[must_use]
    pub fn with_fallback(mut self, reason: FallbackReason) -> Self {
        self.fallback = Some(reason);
        self
    }

    /// True when a fallback produced this text.
    pub fn is_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// Consume the result, keeping only the text.
    pub fn into_text(self) -> String {
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders_compose() {
        let hints = ContextHints::new()
            .with_target_app("Slack")
            .with_style(TextStyle::Message)
            .with_vocabulary(["Iris", "wgpu"])
            .with_locale("en-GB");
        let req = PolishRequest::new("hello").with_hints(hints);

        assert_eq!(req.text, "hello");
        assert_eq!(req.hints.target_app.as_deref(), Some("Slack"));
        assert_eq!(req.hints.style, Some(TextStyle::Message));
        assert_eq!(req.hints.vocabulary, vec!["Iris", "wgpu"]);
        assert_eq!(req.hints.locale.as_deref(), Some("en-GB"));
        assert!(!req.hints.is_empty());
    }

    #[test]
    fn default_hints_are_empty() {
        assert!(ContextHints::default().is_empty());
        assert!(PolishRequest::from("x").hints.is_empty());
    }

    #[test]
    fn polished_tracks_fallback() {
        let p = Polished::new("hi", PolishSource::Rule, Duration::from_micros(3));
        assert!(!p.is_fallback());
        let p = p.with_fallback(FallbackReason::BudgetExceeded { budget_ms: 150 });
        assert!(p.is_fallback());
        assert_eq!(p.into_text(), "hi");
    }
}
