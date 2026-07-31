//! The behaviour corpus, asserted against the deterministic rule engine.
//!
//! Every case in [`iris_polish::corpus::CASES`] is checked twice here: once for
//! its exact expected output (the rule engine is a state machine, so it can be
//! pinned byte-for-byte) and once for the shared properties that any polisher,
//! including a language model, must satisfy.
//!
//! `tests/live_llm.rs` runs the same table against a real endpoint. When these
//! two disagree about a case, the prompt — not the case — is what needs work.

use iris_polish::corpus::{baseline_expectations, Expectation, CASES};
use iris_polish::{MockPolisher, PolishRequest, Polisher, RulePolisher};

#[tokio::test]
async fn rule_polisher_matches_every_expected_output() {
    let polisher = RulePolisher::default();
    let mut failures = Vec::new();

    for case in CASES {
        let output = polisher
            .polish(&PolishRequest::new(case.raw))
            .await
            .expect("the rule engine never fails")
            .text;

        if output != case.rule_output {
            failures.push(format!(
                "case {:?}\n  raw:      {:?}\n  expected: {:?}\n  actual:   {:?}",
                case.name, case.raw, case.rule_output, output
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} cases produced unexpected output:\n\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n\n")
    );
}

#[tokio::test]
async fn rule_polisher_satisfies_every_shared_property() {
    let polisher = RulePolisher::default();
    let mut failures = Vec::new();

    for case in CASES {
        let output = polisher
            .polish(&PolishRequest::new(case.raw))
            .await
            .unwrap()
            .text;

        if let Err(why) = case.check_shared(&output) {
            failures.push(why);
        }
        for expectation in baseline_expectations() {
            if let Err(why) = expectation.check(case.raw, &output) {
                failures.push(format!(
                    "case {:?}: baseline {expectation:?}: {why}",
                    case.name
                ));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Polishing twice must be the same as polishing once. A polisher that keeps
/// editing its own output would drift every time Iris re-polished a buffer.
#[tokio::test]
async fn rule_polisher_is_idempotent_across_the_corpus() {
    let polisher = RulePolisher::default();

    for case in CASES {
        let once = polisher
            .polish(&PolishRequest::new(case.raw))
            .await
            .unwrap()
            .text;
        let twice = polisher
            .polish(&PolishRequest::new(&once))
            .await
            .unwrap()
            .text;
        assert_eq!(twice, once, "case {:?} is not idempotent", case.name);
    }
}

/// The corpus is only meaningful if it can fail. A polisher that mangles text
/// must be caught by it.
#[tokio::test]
async fn the_corpus_rejects_a_bad_polisher() {
    // Ignores its input entirely: the classic "the model answered instead of
    // cleaning" failure.
    let liar = MockPolisher::returning("Sure! Here's a poem about cats for you.");
    let mut caught = 0;

    for case in CASES {
        let output = liar
            .polish(&PolishRequest::new(case.raw))
            .await
            .unwrap()
            .text;
        if case.check_shared(&output).is_err() {
            caught += 1;
        }
    }

    assert!(
        caught >= CASES.len() - 1,
        "the corpus only caught {caught} of {} cases against a polisher that ignores its input",
        CASES.len()
    );
}

/// The same table, run through a mock standing in for a well-behaved LLM, to
/// prove `check_all` (shared + llm_only properties) is wired correctly.
#[tokio::test]
async fn a_well_behaved_polisher_passes_check_all() {
    // A stand-in for the ideal model: exactly what the rules produce, with the
    // llm_only cases hand-corrected the way a model should handle them.
    let ideal = MockPolisher::table([
        ("so, you know, we should ship it", "So we should ship it."),
        (
            "i went to the, i went to the office",
            "I went to the office.",
        ),
        ("did the nightly build pass", "Did the nightly build pass?"),
        (
            "um so uh i was thinking, you know, we could uh cache the the result \
              in redis before 6 pm",
            "So I was thinking we could cache the result in redis before 6 pm.",
        ),
    ]);
    let rules = RulePolisher::default();

    for case in CASES {
        let mocked = ideal
            .polish(&PolishRequest::new(case.raw))
            .await
            .unwrap()
            .text;
        // Anything not in the table falls through to the rule engine's answer,
        // which is what a model should agree with on those cases.
        let output = if mocked == case.raw {
            rules
                .polish(&PolishRequest::new(case.raw))
                .await
                .unwrap()
                .text
        } else {
            mocked
        };

        case.check_all(&output)
            .unwrap_or_else(|why| panic!("{why}"));
    }
}

#[test]
fn every_case_declares_at_least_one_expectation() {
    for case in CASES {
        assert!(
            !case.expect.is_empty(),
            "case {:?} asserts nothing about any polisher",
            case.name
        );
    }
}

#[test]
fn expectation_failures_name_the_case() {
    let case = CASES[0];
    let why = case
        .check_shared("something else entirely")
        .expect_err("the first case should not accept arbitrary text");
    assert!(why.contains(case.name), "{why}");
}

#[test]
fn preservation_is_byte_exact() {
    // Casing counts: "Github" is not "GitHub".
    assert!(Expectation::Preserves("GitHub")
        .check("x", "see GitHub")
        .is_ok());
    assert!(Expectation::Preserves("GitHub")
        .check("x", "see Github")
        .is_err());
}
