//! A [`Polisher`] test double.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::PolishError;
use crate::polisher::{PolishFuture, Polisher};
use crate::types::{PolishRequest, PolishSource, Polished};

/// What a [`MockPolisher`] does with a request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum MockBehavior {
    /// Return the input unchanged.
    Echo,
    /// Return the same text for every input.
    Fixed(String),
    /// Look the input up; fall back to echoing when it is not in the table.
    Table(HashMap<String, String>),
    /// Fail with [`PolishError::Mock`].
    Fail(String),
}

/// A [`Polisher`] with scriptable output and latency.
///
/// The delay is the interesting part: it is how the timeout and fallback
/// behaviour of [`FallbackPolisher`](crate::FallbackPolisher) gets tested
/// without a clock-dependent flake or a network.
///
/// ```
/// use std::time::Duration;
/// use iris_polish::{MockPolisher, PolishRequest, Polisher};
///
/// # async fn demo() {
/// let slow = MockPolisher::returning("Polished.").with_delay(Duration::from_secs(1));
/// // ... hand it to a FallbackPolisher and watch the budget bite.
/// assert_eq!(slow.calls(), 0);
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct MockPolisher {
    behavior: MockBehavior,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

impl MockPolisher {
    /// A mock with the given behaviour and no delay.
    pub fn new(behavior: MockBehavior) -> Self {
        Self {
            behavior,
            delay: Duration::ZERO,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns its input unchanged.
    pub fn echo() -> Self {
        Self::new(MockBehavior::Echo)
    }

    /// Always returns `text`.
    pub fn returning(text: impl Into<String>) -> Self {
        Self::new(MockBehavior::Fixed(text.into()))
    }

    /// Always fails with `message`.
    pub fn failing(message: impl Into<String>) -> Self {
        Self::new(MockBehavior::Fail(message.into()))
    }

    /// Maps specific inputs to specific outputs, echoing anything unlisted.
    pub fn table<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map = entries
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self::new(MockBehavior::Table(map))
    }

    /// Sleep for `delay` before answering.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// How many times `polish` has been called.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Polisher for MockPolisher {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();
            self.calls.fetch_add(1, Ordering::SeqCst);

            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }

            let text = match &self.behavior {
                MockBehavior::Echo => request.text.clone(),
                MockBehavior::Fixed(text) => text.clone(),
                MockBehavior::Table(map) => map
                    .get(&request.text)
                    .cloned()
                    .unwrap_or_else(|| request.text.clone()),
                MockBehavior::Fail(message) => {
                    return Err(PolishError::Mock(message.clone()));
                }
            };

            Ok(Polished::new(text, PolishSource::Mock, started.elapsed()))
        })
    }

    fn expected_latency(&self) -> Option<Duration> {
        Some(self.delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn echo_returns_the_input_and_counts_calls() {
        let mock = MockPolisher::echo();
        let out = run(mock.polish(&PolishRequest::new("as spoken"))).unwrap();
        assert_eq!(out.text, "as spoken");
        assert_eq!(out.source, PolishSource::Mock);
        assert_eq!(mock.calls(), 1);
    }

    #[test]
    fn fixed_ignores_the_input() {
        let mock = MockPolisher::returning("always this");
        let out = run(mock.polish(&PolishRequest::new("anything"))).unwrap();
        assert_eq!(out.text, "always this");
    }

    #[test]
    fn table_maps_known_inputs_and_echoes_the_rest() {
        let mock = MockPolisher::table([("um hi", "Hi.")]);
        assert_eq!(
            run(mock.polish(&PolishRequest::new("um hi"))).unwrap().text,
            "Hi."
        );
        assert_eq!(
            run(mock.polish(&PolishRequest::new("other"))).unwrap().text,
            "other"
        );
        assert_eq!(mock.calls(), 2);
    }

    #[test]
    fn failing_reports_its_message() {
        let mock = MockPolisher::failing("no service");
        let err = run(mock.polish(&PolishRequest::new("x"))).unwrap_err();
        assert!(err.to_string().contains("no service"), "{err}");
    }

    #[test]
    fn delay_is_honoured() {
        let mock = MockPolisher::echo().with_delay(Duration::from_millis(30));
        let started = Instant::now();
        run(mock.polish(&PolishRequest::new("x"))).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn dropping_the_future_cancels_without_running() {
        let mock = MockPolisher::echo().with_delay(Duration::from_secs(30));
        let request = PolishRequest::new("x");
        run(async {
            let future = mock.polish(&request);
            drop(future);
        });
        // The call was counted only if the future was polled; it never was.
        assert_eq!(mock.calls(), 0);
    }
}
