//! Choosing a polisher and running it on the dictation thread.
//!
//! # The choice
//!
//! ```text
//!   polish.enabled = false            → nothing; the transcript is injected verbatim
//!   polish.llm = false, or no key     → RulePolisher alone (offline, ~50 µs)
//!   otherwise                         → FallbackPolisher(LlmPolisher, RulePolisher)
//! ```
//!
//! The third case is the one that matters, and the reason is latency, not
//! quality: [`FallbackPolisher`] computes the rule result up front and then
//! races the LLM against `polish.budget_ms`. The user waits at most the budget
//! and always gets text. See `crates/iris-polish/README.md`.
//!
//! # Why this module blocks
//!
//! [`iris_polish::Polisher`] is async and the dictation loop is not — it is a
//! `select!` over three channels, deliberately, because that is what keeps a
//! frame, a transcript and a key release from ever waiting on a poll tick.
//! Polish is also the one point in the loop where there is nothing else to do:
//! the user has released the key and is waiting for text. So the loop blocks on
//! the shared runtime that [`iris_core`] already keeps for its network engines,
//! rather than growing an async colour it would use exactly once.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use iris_polish::{
    ContextHints, FallbackPolisher, PolishRequest, Polished, Polisher, RulePolisher,
};

use crate::config::Config;

/// Build the polisher `config` asks for, or `None` when polish is off.
///
/// Never fails: a missing key or an unreachable endpoint degrades to the rule
/// engine, which is a complete polisher on its own. Refusing to start a
/// dictation app because a cleanup service is unavailable would be absurd.
pub fn build(config: &Config) -> Option<Arc<dyn Polisher>> {
    if !config.polish.enabled {
        return None;
    }
    let rule: Arc<dyn Polisher> = Arc::new(RulePolisher::default());
    if !config.polish.llm {
        return Some(rule);
    }

    // Both the LLM client and the engines want a rustls provider installed
    // process-wide; calling this twice is a no-op.
    iris_core::engine::net::init_crypto();

    // `LlmPolisher::from_env` builds a reqwest client, which wants to register
    // timers with a running reactor. Entering the runtime first keeps that
    // legal without making this function async.
    let llm = match iris_core::engine::net::runtime() {
        Ok(runtime) => {
            let _guard = runtime.enter();
            iris_polish::LlmPolisher::from_env()
        }
        Err(e) => {
            iris_core::vlog!("polish: no async runtime ({e:#}); using the rule engine");
            return Some(rule);
        }
    };

    match llm {
        Ok(llm) => Some(Arc::new(
            FallbackPolisher::new(Arc::new(llm), Arc::clone(&rule))
                .with_budget(Duration::from_millis(config.polish.budget_ms)),
        )),
        Err(e) => {
            // Almost always "no key", which is a normal, offline-first state.
            iris_core::vlog!("polish: LLM unavailable ({e}); using the rule engine");
            Some(rule)
        }
    }
}

/// The hints the app can supply about where the text is going.
///
/// `target_app` is not filled in yet: reading the focused window's process name
/// is a Windows call that belongs in `iris-core` alongside injection, and it is
/// not worth an upstream change for a hint every current polisher treats as
/// advisory.
pub fn hints(config: &Config) -> ContextHints {
    ContextHints::new().with_style(config.polish.style.into())
}

/// Run `polisher` to completion on the shared runtime.
///
/// Blocks the calling thread, which must not be a runtime worker — the
/// dictation loop's thread never is.
pub fn run(polisher: &dyn Polisher, request: &PolishRequest) -> Result<Polished> {
    let runtime = iris_core::engine::net::runtime()?;
    runtime
        .block_on(polisher.polish(request))
        .context("polishing the transcript")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Style;

    #[test]
    fn polish_off_means_no_polisher() {
        let mut config = Config::default();
        config.polish.enabled = false;
        assert!(build(&config).is_none());
    }

    #[test]
    fn llm_off_pins_the_rule_engine() {
        let mut config = Config::default();
        config.polish.llm = false;
        assert_eq!(build(&config).unwrap().name(), "rule");
    }

    #[test]
    fn the_rule_engine_runs_without_a_runtime_thread_of_its_own() {
        let mut config = Config::default();
        config.polish.llm = false;
        let polisher = build(&config).unwrap();
        let out = run(&*polisher, &PolishRequest::new("um, so i pushed the fix")).unwrap();
        assert_eq!(out.text, "So I pushed the fix.");
        assert!(!out.is_fallback());
    }

    #[test]
    fn style_reaches_the_request_hints() {
        let mut config = Config::default();
        config.polish.style = Style::Technical;
        assert_eq!(
            hints(&config).style,
            Some(iris_polish::TextStyle::Technical)
        );
    }
}
