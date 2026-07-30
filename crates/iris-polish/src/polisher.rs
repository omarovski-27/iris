//! The [`Polisher`] trait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{PolishError, PolishRequest, Polished};

/// The future returned by [`Polisher::polish`].
///
/// Boxed rather than `async fn` in the trait so that `dyn Polisher` works: Iris
/// picks its polisher at runtime from configuration, so trait objects are the
/// whole point.
pub type PolishFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Polished, PolishError>> + Send + 'a>>;

/// Turns a raw transcript into text worth inserting.
///
/// # Cancellation
///
/// The returned future carries no detached work: dropping it cancels the polish
/// and releases everything it held (for [`LlmPolisher`](crate::LlmPolisher), the
/// in-flight HTTP request is aborted). Iris drops the future when the user
/// dismisses the overlay or starts a new utterance.
///
/// # Implementing
///
/// Implementations must be conservative: if a transformation might change what
/// the speaker said, do not make it. Returning the input unchanged is always a
/// valid answer, and is the right one whenever confidence is low.
///
/// ```
/// use std::time::Instant;
/// use iris_polish::{PolishError, PolishFuture, PolishRequest, PolishSource, Polished, Polisher};
///
/// #[derive(Debug)]
/// struct Shouty;
///
/// impl Polisher for Shouty {
///     fn name(&self) -> &'static str {
///         "shouty"
///     }
///
///     fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
///         Box::pin(async move {
///             let started = Instant::now();
///             let text = request.text.to_uppercase();
///             Ok(Polished::new(text, PolishSource::Rule, started.elapsed()))
///         })
///     }
/// }
/// ```
pub trait Polisher: Send + Sync + std::fmt::Debug {
    /// A short, stable identifier used in logs and metrics.
    fn name(&self) -> &'static str;

    /// Clean `request.text`.
    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a>;

    /// The latency this polisher expects to need, if it can say.
    ///
    /// Callers use it to decide whether to even attempt this polisher inside the
    /// remaining budget. `None` means "unknown"; local, allocation-only polishers
    /// should report something near zero.
    fn expected_latency(&self) -> Option<std::time::Duration> {
        None
    }
}

impl<T: Polisher + ?Sized> Polisher for Arc<T> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        (**self).polish(request)
    }

    fn expected_latency(&self) -> Option<std::time::Duration> {
        (**self).expected_latency()
    }
}

impl<T: Polisher + ?Sized> Polisher for Box<T> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        (**self).polish(request)
    }

    fn expected_latency(&self) -> Option<std::time::Duration> {
        (**self).expected_latency()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockPolisher, RulePolisher};

    #[test]
    fn trait_is_object_safe_and_forwards_through_smart_pointers() {
        let boxed: Box<dyn Polisher> = Box::new(RulePolisher::default());
        assert_eq!(boxed.name(), "rule");

        let arced: Arc<dyn Polisher> = Arc::new(MockPolisher::echo());
        assert_eq!(arced.name(), "mock");
        assert_eq!(Polisher::name(&arced), "mock");
    }
}
