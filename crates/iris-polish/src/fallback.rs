//! Latency discipline: budget enforcement with an instant fallback.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::PolishError;
use crate::polisher::{PolishFuture, Polisher};
use crate::types::{FallbackReason, PolishRequest};

/// The polish step's latency budget: 150 ms.
///
/// Iris targets well under 300 ms from key-release to text on screen, and polish
/// is only one stage of that. 150 ms is what is left once the tail of
/// transcription and the insertion path have taken their share.
pub const DEFAULT_LATENCY_BUDGET: Duration = Duration::from_millis(150);

/// Runs a good-but-slow polisher against the clock, backed by an instant one.
///
/// # How it behaves
///
/// The fallback runs **first**, unconditionally. That looks wasteful until you
/// notice that the intended fallback — [`RulePolisher`](crate::RulePolisher) —
/// costs microseconds, and that paying it up front means the deadline path has
/// an answer already in hand and never has to do work *after* the budget has
/// already been spent. The user's worst case is the budget, not the budget plus
/// a second polish.
///
/// Then the primary races the budget:
///
/// | Outcome | Result |
/// |---|---|
/// | primary finishes in time | its text, `source` from the primary |
/// | primary exceeds the budget | fallback text, `fallback = BudgetExceeded` |
/// | primary errors | fallback text, `fallback = PolisherFailed` |
/// | both fail | the primary's error |
///
/// [`Polished::duration`](crate::Polished::duration) is total wall-clock
/// through this polisher, so callers
/// see what the user actually waited for, not what the winning stage cost.
///
/// # Contract
///
/// The fallback must be fast and must not fail. Anything that does I/O belongs
/// in the primary slot.
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use iris_polish::{FallbackPolisher, MockPolisher, PolishRequest, Polisher, RulePolisher};
///
/// # async fn demo() {
/// let slow = MockPolisher::returning("never arrives").with_delay(Duration::from_secs(5));
/// let polisher = FallbackPolisher::new(Arc::new(slow), Arc::new(RulePolisher::default()))
///     .with_budget(Duration::from_millis(20));
///
/// let out = polisher.polish(&PolishRequest::new("um it still works")).await.unwrap();
/// assert_eq!(out.text, "It still works.");
/// assert!(out.is_fallback());
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct FallbackPolisher {
    primary: Arc<dyn Polisher>,
    fallback: Arc<dyn Polisher>,
    budget: Duration,
}

impl FallbackPolisher {
    /// Race `primary` against [`DEFAULT_LATENCY_BUDGET`], backed by `fallback`.
    pub fn new(primary: Arc<dyn Polisher>, fallback: Arc<dyn Polisher>) -> Self {
        Self {
            primary,
            fallback,
            budget: DEFAULT_LATENCY_BUDGET,
        }
    }

    /// Set the budget the primary must finish inside.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// The budget in force.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// The polisher tried first.
    pub fn primary(&self) -> &Arc<dyn Polisher> {
        &self.primary
    }

    /// The polisher used when the primary misses or fails.
    pub fn fallback(&self) -> &Arc<dyn Polisher> {
        &self.fallback
    }
}

impl Polisher for FallbackPolisher {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();

            // Insurance, bought up front while it is cheap.
            let safety_net = self.fallback.polish(request).await;

            let remaining = self.budget.saturating_sub(started.elapsed());
            let outcome = if remaining.is_zero() {
                Err(PolishError::BudgetExceeded {
                    budget_ms: self.budget.as_millis() as u64,
                })
            } else {
                match tokio::time::timeout(remaining, self.primary.polish(request)).await {
                    Ok(result) => result,
                    Err(_) => Err(PolishError::BudgetExceeded {
                        budget_ms: self.budget.as_millis() as u64,
                    }),
                }
            };

            match outcome {
                Ok(mut polished) => {
                    polished.duration = started.elapsed();
                    Ok(polished)
                }
                Err(error) => {
                    let reason = match &error {
                        PolishError::BudgetExceeded { budget_ms } => {
                            tracing::debug!(
                                budget_ms = *budget_ms,
                                primary = self.primary.name(),
                                "polish budget exceeded, using fallback"
                            );
                            FallbackReason::BudgetExceeded {
                                budget_ms: *budget_ms,
                            }
                        }
                        other => {
                            tracing::debug!(
                                primary = self.primary.name(),
                                error = %other,
                                "primary polisher failed, using fallback"
                            );
                            FallbackReason::PolisherFailed(other.to_string())
                        }
                    };

                    match safety_net {
                        Ok(mut polished) => {
                            polished.duration = started.elapsed();
                            Ok(polished.with_fallback(reason))
                        }
                        // Both failed. The primary's error is the informative one.
                        Err(_) => Err(error),
                    }
                }
            }
        })
    }

    fn expected_latency(&self) -> Option<Duration> {
        Some(self.budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PolishSource;
    use crate::{MockPolisher, RulePolisher};

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn primary_wins_when_it_is_fast_enough() {
        let polisher = FallbackPolisher::new(
            Arc::new(MockPolisher::returning("From the model.")),
            Arc::new(RulePolisher::default()),
        );
        let out = run(polisher.polish(&PolishRequest::new("um from the model"))).unwrap();

        assert_eq!(out.text, "From the model.");
        assert_eq!(out.source, PolishSource::Mock);
        assert!(!out.is_fallback());
    }

    #[test]
    fn budget_breach_falls_back_to_the_rule_engine() {
        let slow = MockPolisher::returning("too late").with_delay(Duration::from_secs(30));
        let polisher = FallbackPolisher::new(Arc::new(slow), Arc::new(RulePolisher::default()))
            .with_budget(Duration::from_millis(20));

        let started = Instant::now();
        let out = run(polisher.polish(&PolishRequest::new("um it still works"))).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(out.text, "It still works.");
        assert_eq!(out.source, PolishSource::Rule);
        assert_eq!(
            out.fallback,
            Some(FallbackReason::BudgetExceeded { budget_ms: 20 })
        );
        assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
    }

    #[test]
    fn a_failing_primary_falls_back() {
        let polisher = FallbackPolisher::new(
            Arc::new(MockPolisher::failing("endpoint down")),
            Arc::new(RulePolisher::default()),
        );
        let out = run(polisher.polish(&PolishRequest::new("um it still works"))).unwrap();

        assert_eq!(out.text, "It still works.");
        match out.fallback {
            Some(FallbackReason::PolisherFailed(ref why)) => {
                assert!(why.contains("endpoint down"), "{why}")
            }
            other => panic!("expected PolisherFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_budget_never_calls_the_primary() {
        let primary = Arc::new(MockPolisher::returning("unused"));
        let polisher = FallbackPolisher::new(primary.clone(), Arc::new(RulePolisher::default()))
            .with_budget(Duration::ZERO);

        let out = run(polisher.polish(&PolishRequest::new("um skip it"))).unwrap();
        assert_eq!(out.text, "Skip it.");
        assert_eq!(primary.calls(), 0);
        assert!(out.is_fallback());
    }

    #[test]
    fn both_failing_surfaces_the_primary_error() {
        let polisher = FallbackPolisher::new(
            Arc::new(MockPolisher::failing("primary broke")),
            Arc::new(MockPolisher::failing("fallback broke")),
        );
        let err = run(polisher.polish(&PolishRequest::new("x"))).unwrap_err();
        assert!(err.to_string().contains("primary broke"), "{err}");
    }

    #[test]
    fn duration_is_total_wall_clock_not_the_winning_stage() {
        let slow = MockPolisher::returning("late").with_delay(Duration::from_millis(60));
        let polisher = FallbackPolisher::new(Arc::new(slow), Arc::new(RulePolisher::default()))
            .with_budget(Duration::from_millis(200));

        let out = run(polisher.polish(&PolishRequest::new("um hello"))).unwrap();
        assert_eq!(out.text, "late");
        assert!(
            out.duration >= Duration::from_millis(55),
            "duration {:?} did not include the primary's latency",
            out.duration
        );
    }

    #[test]
    fn the_user_never_waits_much_longer_than_the_budget() {
        let slow = MockPolisher::returning("late").with_delay(Duration::from_secs(30));
        let polisher = FallbackPolisher::new(Arc::new(slow), Arc::new(RulePolisher::default()))
            .with_budget(Duration::from_millis(50));

        let out = run(polisher.polish(&PolishRequest::new("um hello there"))).unwrap();
        assert!(
            out.duration < Duration::from_millis(500),
            "spent {:?} on a 50 ms budget",
            out.duration
        );
    }

    #[test]
    fn fallbacks_can_be_chained() {
        // A realistic three-tier stack: model, then a cheaper model, then rules.
        let inner = FallbackPolisher::new(
            Arc::new(MockPolisher::failing("primary down")),
            Arc::new(RulePolisher::default()),
        );
        let outer = FallbackPolisher::new(
            Arc::new(MockPolisher::failing("secondary down")),
            Arc::new(inner),
        );
        let out = run(outer.polish(&PolishRequest::new("um chained"))).unwrap();
        assert_eq!(out.text, "Chained.");
        assert!(out.is_fallback());
    }
}
