//! Classifying *why* a dictation could not reach or use a transcription
//! service, instead of collapsing every cause into one generic message.
//!
//! Before this module, a rejected API key, an exhausted balance, a rate
//! limit and a dead network connection all produced the same shape of
//! message — whatever `anyhow::Context` string happened to be on the `?` that
//! failed — so a captain out of Deepgram credit saw the identical text a
//! captain with no wifi would see, and went looking for a router problem
//! that did not exist.
//!
//! [`FailureCause`] is the fix: a small, provider-agnostic classification
//! [`deepgram`](super::deepgram) and [`groq`](super::groq) each derive from
//! their own provider's documented status codes (see the classification
//! functions in each module for the citations), paired with one shared,
//! actionable message per cause via [`FailureCause::message`]. [`Failure`] is
//! the vehicle that carries a classified cause (or, for anything this module
//! cannot confidently name, an ordinary [`anyhow::Error`]) out of an engine's
//! connect/request path and into a [`super::TranscriptEvent::Failed`] or
//! [`super::TranscriptEvent::Error`] — see [`Failure::into_event`].
//!
//! Deliberately never reads or repeats the API key: every classification
//! here works from an HTTP status code and (optionally) the response body or
//! an I/O error's own text, none of which the provider echoes the request's
//! `Authorization` header into.

use std::fmt;

/// Why an engine could not reach or use a transcription service, coarse
/// enough to be provider-agnostic but specific enough to tell the user (and
/// the session log) the true story instead of one generic message.
///
/// `Unknown` is the deliberate, safe default for anything this module cannot
/// confidently classify — a status code neither provider's docs assign a
/// meaning to, a malformed response, a TLS or protocol error. It must never
/// be stretched to claim a cause the evidence does not support; see each
/// classification function's own doc comment for exactly what evidence maps
/// to what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    /// The configured API key was rejected outright, or is valid but not
    /// entitled to what was requested. Both fold into one cause because the
    /// fix a user can take is the same either way: check the key and the
    /// account it belongs to — never "check your internet connection".
    InvalidKey,
    /// The account has run out of credit or otherwise has a billing problem.
    ExhaustedCredit,
    /// The provider is rate-limiting this key. Unlike the two causes above,
    /// this one clears on its own with time.
    RateLimited,
    /// No route to the provider at all — DNS failure, connection refused, no
    /// network. Also self-clearing once connectivity returns.
    NetworkUnreachable,
    /// The connection attempt did not complete within this engine's own
    /// connect budget.
    Timeout,
    /// Real failure, no confident classification. Never implies a specific
    /// fix the evidence does not support.
    Unknown,
}

impl FailureCause {
    /// Whether trying again without the user changing anything might
    /// succeed. An invalid key or an empty balance will not fix itself —
    /// retrying wastes time and hides the message that would have told the
    /// user what to do — whereas a rate limit or a network blip may clear on
    /// its own.
    ///
    /// Iris does not currently retry a dictation automatically anywhere in
    /// its capture path, so nothing calls this today; it exists so the
    /// classification is unit-testable end to end and so a future retry
    /// mechanism (if one is ever built) does not have to re-derive this
    /// answer.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(
            self,
            FailureCause::RateLimited | FailureCause::NetworkUnreachable
        )
    }

    /// A short, stable, greppable label for the session log — see
    /// `iris_app::history::DictationRecord::connection_cause`. Never the
    /// user-facing message: that can be reworded freely without breaking a
    /// log line written by an older build.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            FailureCause::InvalidKey => "invalid_key",
            FailureCause::ExhaustedCredit => "exhausted_credit",
            FailureCause::RateLimited => "rate_limited",
            FailureCause::NetworkUnreachable => "network_unreachable",
            FailureCause::Timeout => "timeout",
            FailureCause::Unknown => "unknown",
        }
    }

    /// The plain, actionable sentence shown to the user (via the dialog
    /// `iris_app::notify::FailureNotice::connection_failed` already shows)
    /// and recorded as `iris_app::history::DictationRecord::error`.
    ///
    /// `provider` names the service ("Deepgram", "Groq") so the message never
    /// makes the user guess which account to check. `key_env` is the
    /// environment variable that carries the key, `console_url` is where to
    /// manage the account — both engine-specific, both already known to
    /// `EngineOptions`'s callers via [`super::require_key`]. `detail` is
    /// optional extra evidence (an HTTP status, a truncated response body, an
    /// I/O error's own text) appended for anyone diagnosing from the session
    /// log; it is never the API key, since none of this module's callers ever
    /// pass the key or a request that could echo it back.
    #[must_use]
    pub fn message(
        self,
        provider: &str,
        key_env: &str,
        console_url: &str,
        detail: Option<&str>,
    ) -> String {
        let suffix = detail.map(|d| format!(" ({d})")).unwrap_or_default();
        match self {
            FailureCause::InvalidKey => format!(
                "{provider} rejected the configured API key{suffix}. Check {key_env} — and that \
                 your {provider} account has access to the model Iris requests — at {console_url}, \
                 then try again."
            ),
            FailureCause::ExhaustedCredit => format!(
                "Your {provider} balance has run out{suffix}. Add credit at {console_url} to keep \
                 dictating."
            ),
            FailureCause::RateLimited => format!(
                "{provider} is rate-limiting requests right now{suffix}. Wait a moment and try \
                 again."
            ),
            FailureCause::NetworkUnreachable => format!(
                "Iris could not reach {provider}{suffix}. Check your internet connection and try \
                 again."
            ),
            FailureCause::Timeout => format!(
                "Connecting to {provider} timed out{suffix}. Check your internet connection and \
                 try again."
            ),
            FailureCause::Unknown => format!("{provider} could not be reached{suffix}."),
        }
    }
}

/// A connect/request failure an engine's async task hands back to its own
/// `open`/`finish` wrapper, on the way to becoming a
/// [`super::TranscriptEvent`] — see [`Failure::into_event`].
///
/// Exists so `deepgram::pump_inner` and `groq::transcribe` can keep using
/// ordinary `anyhow::Context` and `?` for the errors this module has no
/// opinion about (`impl From<anyhow::Error> for Failure` makes that
/// transparent), while the one or two call sites in each that *can* name a
/// specific cause opt in explicitly with [`Failure::Classified`].
pub(crate) enum Failure {
    /// A specific, actionable cause — the message is already fully composed
    /// (see [`FailureCause::message`]) so the event carrying it needs no
    /// further formatting.
    Classified {
        message: String,
        cause: FailureCause,
    },
    /// Everything else: an ordinary error this module cannot confidently
    /// classify, printed the same way every other engine failure always has
    /// been.
    Other(anyhow::Error),
}

impl Failure {
    /// The `TranscriptEvent` this failure becomes. `Classified` produces
    /// [`super::TranscriptEvent::Failed`] so [`crate::dictation::Dictation`]
    /// can carry the cause through to the app layer; `Other` produces the
    /// plain [`super::TranscriptEvent::Error`] every engine failure has
    /// always produced.
    pub(crate) fn into_event(self) -> super::TranscriptEvent {
        match self {
            Failure::Classified { message, cause } => {
                super::TranscriptEvent::Failed { message, cause }
            }
            Failure::Other(e) => super::TranscriptEvent::Error(format!("{e:#}")),
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Other(e)
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Classified { message, cause } => f
                .debug_struct("Classified")
                .field("cause", cause)
                .field("message", message)
                .finish(),
            Failure::Other(e) => f.debug_tuple("Other").field(&format!("{e:#}")).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rate_limits_and_network_gaps_are_treated_as_retryable() {
        assert!(FailureCause::RateLimited.retryable());
        assert!(FailureCause::NetworkUnreachable.retryable());
        assert!(!FailureCause::InvalidKey.retryable());
        assert!(!FailureCause::ExhaustedCredit.retryable());
        assert!(!FailureCause::Timeout.retryable());
        assert!(!FailureCause::Unknown.retryable());
    }

    #[test]
    fn labels_are_stable_greppable_snake_case() {
        assert_eq!(FailureCause::InvalidKey.label(), "invalid_key");
        assert_eq!(FailureCause::ExhaustedCredit.label(), "exhausted_credit");
        assert_eq!(FailureCause::RateLimited.label(), "rate_limited");
        assert_eq!(
            FailureCause::NetworkUnreachable.label(),
            "network_unreachable"
        );
        assert_eq!(FailureCause::Timeout.label(), "timeout");
        assert_eq!(FailureCause::Unknown.label(), "unknown");
    }

    #[test]
    fn every_cause_names_the_provider_and_never_says_check_the_key_unless_it_might_be_the_key() {
        for cause in [
            FailureCause::InvalidKey,
            FailureCause::ExhaustedCredit,
            FailureCause::RateLimited,
            FailureCause::NetworkUnreachable,
            FailureCause::Timeout,
            FailureCause::Unknown,
        ] {
            let msg = cause.message(
                "Deepgram",
                "IRIS_DEEPGRAM_KEY",
                "https://console.deepgram.com",
                None,
            );
            assert!(
                msg.contains("Deepgram"),
                "{cause:?} message drops the provider name: {msg}"
            );
            let mentions_key =
                msg.contains("IRIS_DEEPGRAM_KEY") || msg.to_lowercase().contains("key");
            assert_eq!(
                mentions_key,
                cause == FailureCause::InvalidKey,
                "{cause:?} message wrongly {} the key: {msg}",
                if mentions_key { "mentions" } else { "omits" }
            );
        }
    }

    #[test]
    fn exhausted_credit_names_the_fix_not_the_symptom() {
        let msg = FailureCause::ExhaustedCredit.message(
            "Deepgram",
            "IRIS_DEEPGRAM_KEY",
            "https://console.deepgram.com",
            Some("HTTP 402"),
        );
        assert!(msg.contains("credit"), "{msg}");
        assert!(msg.contains("https://console.deepgram.com"), "{msg}");
        assert!(
            msg.contains("HTTP 402"),
            "the raw evidence should still be there for diagnosis: {msg}"
        );
    }

    #[test]
    fn the_detail_suffix_is_omitted_entirely_when_there_is_none() {
        let msg = FailureCause::RateLimited.message(
            "Groq",
            "IRIS_GROQ_KEY",
            "https://console.groq.com/keys",
            None,
        );
        assert!(
            !msg.contains("()"),
            "an absent detail must not leave empty parens: {msg}"
        );
    }

    #[test]
    fn a_classified_failure_becomes_the_failed_event_unmodified() {
        let event = Failure::Classified {
            message: "boom".into(),
            cause: FailureCause::RateLimited,
        }
        .into_event();
        assert_eq!(
            event,
            super::super::TranscriptEvent::Failed {
                message: "boom".into(),
                cause: FailureCause::RateLimited
            }
        );
    }

    #[test]
    fn an_unclassified_failure_becomes_a_plain_error_event() {
        let event = Failure::Other(anyhow::anyhow!("socket died")).into_event();
        match event {
            super::super::TranscriptEvent::Error(msg) => assert!(msg.contains("socket died")),
            other => panic!("expected a plain Error event, got {other:?}"),
        }
    }

    #[test]
    fn anyhow_errors_convert_into_the_unclassified_variant() {
        let failure: Failure = anyhow::anyhow!("whatever").into();
        assert!(matches!(failure, Failure::Other(_)));
    }
}
