//! Manual smoke test for the polisher stack.
//!
//! ```text
//! cargo run --example polish -- "um so uh i think the the fix works"
//! echo "um hello" | cargo run --example polish
//! ```
//!
//! By default it uses the LLM path when a key is present and the rule engine
//! otherwise — exactly the decision Iris itself makes at startup. `--rule`
//! forces the offline path; `--budget-ms` moves the deadline.

use std::io::Read;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use iris_polish::{ContextHints, PolishRequest, Polisher, RulePolisher, TextStyle};

const USAGE: &str = "\
usage: polish [OPTIONS] [TEXT...]

Reads TEXT from the arguments, or from stdin when there are none.

Options:
  --rule              force the offline rule engine, even if a key is set
  --budget-ms MS      latency budget for the LLM path (default 150)
  --app NAME          target-app hint, e.g. --app Slack
  --style STYLE       prose | message | technical
  --term TERM         a term that must survive verbatim (repeatable)
  -h, --help          show this message

Environment:
  IRIS_GROQ_KEY / IRIS_LLM_KEY    API key; without one the rule engine is used
  IRIS_LLM_BASE_URL               default https://api.groq.com/openai/v1
  IRIS_LLM_MODEL                  default llama-3.1-8b-instant
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let text = match options.text {
        Some(ref text) => text.clone(),
        None => match read_stdin() {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: could not read stdin: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    if text.trim().is_empty() {
        eprintln!("error: no input text\n\n{USAGE}");
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(run(options.force_rule, options.budget, options.hints, text))
}

async fn run(force_rule: bool, budget: Duration, hints: ContextHints, text: String) -> ExitCode {
    let polisher = build_polisher(force_rule, budget);
    let request = PolishRequest::new(&text).with_hints(hints);

    match polisher.polish(&request).await {
        Ok(polished) => {
            println!("{}", polished.text);
            eprintln!(
                "--- {} in {:.1} ms{}",
                polished.source,
                polished.duration.as_secs_f64() * 1000.0,
                polished
                    .fallback
                    .map(|r| format!(" (fell back: {r})"))
                    .unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "llm")]
fn build_polisher(force_rule: bool, budget: Duration) -> Arc<dyn Polisher> {
    use iris_polish::{FallbackPolisher, LlmConfig, LlmPolisher};

    if force_rule {
        return Arc::new(RulePolisher::default());
    }

    let config = match LlmConfig::from_env() {
        Ok(config) => config.with_timeout(budget),
        Err(e) => {
            eprintln!("--- {e}; using the rule engine");
            return Arc::new(RulePolisher::default());
        }
    };

    match LlmPolisher::new(config) {
        Ok(llm) => Arc::new(
            FallbackPolisher::new(Arc::new(llm), Arc::new(RulePolisher::default()))
                .with_budget(budget),
        ),
        Err(e) => {
            eprintln!("--- {e}; using the rule engine");
            Arc::new(RulePolisher::default())
        }
    }
}

#[cfg(not(feature = "llm"))]
fn build_polisher(_force_rule: bool, _budget: Duration) -> Arc<dyn Polisher> {
    Arc::new(RulePolisher::default())
}

struct Options {
    text: Option<String>,
    force_rule: bool,
    budget: Duration,
    hints: ContextHints,
}

/// `Ok(None)` means `--help` was asked for.
fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut options = Options {
        text: None,
        force_rule: false,
        budget: iris_polish::DEFAULT_LATENCY_BUDGET,
        hints: ContextHints::new(),
    };
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(None),
            "--rule" => options.force_rule = true,
            "--budget-ms" => {
                let raw = next(args, &mut i, "--budget-ms")?;
                let ms: u64 = raw
                    .parse()
                    .map_err(|_| format!("--budget-ms wants a number, got {raw:?}"))?;
                options.budget = Duration::from_millis(ms);
            }
            "--app" => {
                let app = next(args, &mut i, "--app")?;
                options.hints = std::mem::take(&mut options.hints).with_target_app(app);
            }
            "--style" => {
                let raw = next(args, &mut i, "--style")?;
                let style = match raw.to_lowercase().as_str() {
                    "prose" => TextStyle::Prose,
                    "message" => TextStyle::Message,
                    "technical" => TextStyle::Technical,
                    other => return Err(format!("unknown style {other:?}")),
                };
                options.hints = std::mem::take(&mut options.hints).with_style(style);
            }
            "--term" => {
                let term = next(args, &mut i, "--term")?;
                options.hints = std::mem::take(&mut options.hints).with_vocabulary([term]);
            }
            other if other.starts_with("--") => return Err(format!("unknown option {other:?}")),
            other => words.push(other.to_string()),
        }
        i += 1;
    }

    if !words.is_empty() {
        options.text = Some(words.join(" "));
    }
    Ok(Some(options))
}

fn next(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn read_stdin() -> std::io::Result<String> {
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer)
}
