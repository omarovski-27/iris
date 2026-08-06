//! What the terminal actually shows, driven through the real `iris` binary.
//!
//! The console is quiet by default: a dictation prints its result and nothing
//! else. The per-span latency table is a developer tool and lives behind
//! `--report`; diagnostics live behind `--verbose` and go to stderr. The dev
//! paths (`--demo-dictation`, `--speak-wav`) follow the same rule as the
//! resident loop, which is the part that regressed before.
//!
//! `--demo-dictation` is safe to run unattended: it forces the mock engine and
//! a dry-run injector, so there is no network and no `SendInput`.
//!
//! This is also, incidentally, coverage for the other half of the
//! console-output contract: `Command::output()` below pipes stdout/stderr,
//! which gives the child real, already-inherited handles the same way
//! redirecting to a file does (`iris.exe > out.txt`). `main.rs`'s
//! `attach_console_for_cli_output` is written to leave already-wired stdio
//! alone rather than tear it up with a console handle — if that check ever
//! regressed, every assertion below would start reading empty output instead
//! of a real transcript, on any platform.

use std::process::Command;

/// Lines only a developer wants: they must never appear unasked-for.
const TABLE_MARKERS: [&str; 4] = [
    "PERCEIVED",
    "── dictation #",
    "key-release → final transcript",
    "partials",
];

struct Run {
    stdout: String,
    stderr: String,
}

/// One `--demo-dictation` run with an isolated config, so the developer's own
/// settings and session log are never read or written.
fn demo(extra: &[&str]) -> Run {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_iris"))
        .arg("--demo-dictation")
        .arg("--config")
        .arg(dir.path().join("config.toml"))
        .args(extra)
        .output()
        .expect("running the iris binary");
    assert!(
        output.status.success(),
        "iris exited with {}",
        output.status
    );
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn a_dictation_prints_its_result_and_no_instrumentation() {
    let run = demo(&[]);

    assert!(
        run.stdout.contains("demo: "),
        "the transcript line is the point of the run:\n{}",
        run.stdout
    );
    for marker in TABLE_MARKERS {
        assert!(
            !run.stdout.contains(marker),
            "{marker:?} is developer instrumentation and must stay behind --report:\n{}",
            run.stdout
        );
    }
    assert!(
        !run.stdout.contains(" ms"),
        "no millisecond figures without --report:\n{}",
        run.stdout
    );
    assert!(
        run.stderr.is_empty(),
        "nothing goes to stderr when nothing went wrong:\n{}",
        run.stderr
    );
}

#[test]
fn report_still_prints_the_full_latency_table() {
    let run = demo(&["--report"]);

    for marker in TABLE_MARKERS {
        assert!(
            !marker.is_empty() && run.stdout.contains(marker),
            "--report asked for the table, so {marker:?} must be in it:\n{}",
            run.stdout
        );
    }
    assert!(
        run.stdout.contains("demo: "),
        "the result line survives --report too:\n{}",
        run.stdout
    );
}

#[test]
fn verbose_adds_diagnostics_on_stderr_without_reopening_stdout() {
    let run = demo(&["--verbose"]);

    assert!(
        run.stderr.contains("[iris]"),
        "--verbose is what turns iris_core::vlog! on:\n{}",
        run.stderr
    );
    for marker in TABLE_MARKERS {
        assert!(
            !run.stdout.contains(marker),
            "--verbose is not --report; {marker:?} must stay off stdout:\n{}",
            run.stdout
        );
    }
}
