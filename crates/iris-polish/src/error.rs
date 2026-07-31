//! Failure modes of a polish attempt.

/// Everything that can go wrong while polishing.
///
/// Callers are expected to treat any of these as "use the rule engine instead"
/// rather than "show the user an error": losing the dictated text because a
/// cleanup step failed is never the right outcome.
/// [`FallbackPolisher`](crate::FallbackPolisher) automates exactly that.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolishError {
    /// No API key in the environment, so the LLM path cannot run at all.
    #[error(
        "no LLM API key: set IRIS_GROQ_KEY (or IRIS_LLM_KEY). \
         Without one, use RulePolisher, which needs no network"
    )]
    MissingApiKey,

    /// The configuration is unusable (empty model, malformed base URL, ...).
    #[error("invalid LLM configuration: {0}")]
    Config(String),

    /// The HTTP request never produced a response: DNS, TLS, connection, or a
    /// dropped socket.
    #[error("LLM request failed: {0}")]
    Transport(String),

    /// The endpoint answered with a non-success status.
    #[error("LLM endpoint returned HTTP {status}: {body}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// The response body, truncated for logging.
        body: String,
    },

    /// The response arrived but was not a usable chat completion.
    #[error("could not read LLM response: {0}")]
    Response(String),

    /// The model answered, but its output failed a safety guard, so it was
    /// discarded rather than shown to the user.
    ///
    /// This is the crate's core promise doing its job: a rewrite that might have
    /// changed meaning is thrown away in favour of the conservative path.
    #[error("rejected LLM output: {0}")]
    Rejected(String),

    /// The polish did not finish inside its latency budget.
    #[error("polish exceeded its {budget_ms} ms budget")]
    BudgetExceeded {
        /// The budget, in milliseconds.
        budget_ms: u64,
    },

    /// A test double was configured to fail.
    #[error("mock polisher failed: {0}")]
    Mock(String),
}

impl PolishError {
    /// True when retrying the same request could plausibly succeed.
    ///
    /// Iris does not retry on the interactive path — there is no budget for a
    /// second round trip — but background re-polish and diagnostics use this.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::BudgetExceeded { .. } => true,
            Self::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_message_names_both_env_vars() {
        let msg = PolishError::MissingApiKey.to_string();
        assert!(msg.contains("IRIS_GROQ_KEY"), "{msg}");
        assert!(msg.contains("IRIS_LLM_KEY"), "{msg}");
    }

    #[test]
    fn transience_classification() {
        assert!(PolishError::Transport("reset".into()).is_transient());
        assert!(PolishError::BudgetExceeded { budget_ms: 150 }.is_transient());
        assert!(PolishError::HttpStatus {
            status: 503,
            body: String::new()
        }
        .is_transient());
        assert!(PolishError::HttpStatus {
            status: 429,
            body: String::new()
        }
        .is_transient());

        assert!(!PolishError::MissingApiKey.is_transient());
        assert!(!PolishError::Rejected("grew too much".into()).is_transient());
        assert!(!PolishError::HttpStatus {
            status: 401,
            body: String::new()
        }
        .is_transient());
    }
}
