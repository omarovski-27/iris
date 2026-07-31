//! The README documents the exact system prompt. This keeps it honest.
//!
//! Prompt engineering is only reviewable if the documented prompt is the one that
//! ships. A copy-paste that silently rots is worse than no documentation, because
//! the next person reasons about a prompt that is not running.

#![cfg(feature = "llm")]

use iris_polish::SYSTEM_PROMPT;

const README: &str = include_str!("../README.md");

#[test]
fn the_readme_quotes_the_system_prompt_verbatim() {
    assert!(
        README.contains(SYSTEM_PROMPT),
        "the system prompt in README.md has drifted from iris_polish::SYSTEM_PROMPT.\n\
         Re-copy the constant into the ```text block under \"The exact system prompt\".\n\n\
         Current constant:\n{SYSTEM_PROMPT}"
    );
}

#[test]
fn the_readme_documents_the_environment_variables() {
    for name in [
        "IRIS_GROQ_KEY",
        "IRIS_LLM_KEY",
        "IRIS_LLM_BASE_URL",
        "IRIS_LLM_MODEL",
        "IRIS_LLM_TIMEOUT_MS",
        "IRIS_LIVE_LLM_TESTS",
    ] {
        assert!(README.contains(name), "README.md does not mention {name}");
    }
}

#[test]
fn the_readme_documents_failure_modes() {
    assert!(
        README.contains("## Known failure modes"),
        "README.md must document known failure modes"
    );
}
