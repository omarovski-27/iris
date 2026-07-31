//! Timing and easing constants.
//!
//! These are acceptance criteria, not preferences. They come from the Prism
//! mockup's own spec card (`c-prism/index.html`, section 05) and from the
//! design report's shared product constraints. Prism is the locked v1
//! direction, so its timings are the *single* set — Porcelain is a token swap
//! and does not get its own motion budget.
//!
//! ```text
//! enter 130 ms  ·  exit 150 ms  ·  state cross 90 ms
//! check draw 240 ms  ·  inserted hold 550 ms
//! transform + opacity only  ·  28 bars x scaleY  ·  no blur filters
//! ```

/// Hidden → visible. The snappiest of the three directions, and the reason
/// Prism won: the report's shared budget allows ≤ 160 ms and this spends 130.
pub const ENTER_MS: u32 = 130;

/// Visible → hidden.
pub const EXIT_MS: u32 = 150;

/// Cross-fade between two *visible* states (listening → processing →
/// inserted). Short enough that the pill never looks like it is thinking about
/// it; long enough that the capsule swap is not a pop.
pub const STATE_CROSS_MS: u32 = 90;

/// How long the inserted check takes to draw itself.
pub const CHECK_DRAW_MS: u32 = 240;

/// How long `inserted` stays on screen before the exit animation starts.
///
/// The Prism spec card says 500 ms and Porcelain's says 600 ms; the report's
/// shared constraint is "inserted hold ~500–600 ms then exit". The captain's
/// brief locked ~550 ms, the midpoint, and that is what ships. Total time from
/// `inserted()` to a blank screen is therefore `INSERTED_HOLD_MS + EXIT_MS`.
pub const INSERTED_HOLD_MS: u32 = 550;

/// Period of the live-core listening pulse.
pub const REC_PULSE_MS: u32 = 1400;

/// Period of one full turn of the processing spinner.
pub const SPINNER_PERIOD_MS: u32 = 580;

/// Period of one sweep of the processing scan band.
pub const SCAN_PERIOD_MS: u32 = 720;

/// Frame interval the window loop paces to while the pill is on screen.
pub const FRAME_INTERVAL_MS: u64 = 16;

/// How long the window loop blocks waiting for a command while the pill is
/// hidden. Long enough to cost nothing, short enough that the message pump
/// still sees `WM_DPICHANGED` / `WM_DISPLAYCHANGE` promptly.
pub const IDLE_POLL_MS: u64 = 100;

/// How many characters of partial transcript fill the partial ribbon.
pub const RIBBON_FULL_CHARS: usize = 120;

/// Attack time constant for the incoming microphone level, in ms.
///
/// Level arrives from the audio thread as a noisy RMS. Rising fast and falling
/// slower is what makes a meter read as "responsive" instead of "jittery".
pub const LEVEL_ATTACK_MS: f32 = 25.0;

/// Release time constant for the incoming microphone level, in ms.
pub const LEVEL_RELEASE_MS: f32 = 120.0;

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function.
///
/// Both control points' x coordinates must be in 0..=1 for the curve to be a
/// function of time; the constructors below are the two curves the mockups use.
#[derive(Clone, Copy, Debug)]
pub struct Ease {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// `cubic-bezier(.2, .9, .2, 1)` — Prism's `--in`. Overshoot-free but very
/// front-loaded: most of the distance is covered in the first third.
pub const EASE_IN: Ease = Ease::new(0.2, 0.9, 0.2, 1.0);

/// `cubic-bezier(.4, 0, .9, .3)` — Prism's `--out`. Slow to leave, then quick.
pub const EASE_OUT: Ease = Ease::new(0.4, 0.0, 0.9, 0.3);

impl Ease {
    /// Build a timing function from its two control points.
    #[must_use]
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Evaluate the curve at time `t` (clamped to 0.0–1.0).
    #[must_use]
    pub fn eval(&self, t: f32) -> f32 {
        // NaN falls through to 0: an easing curve is not the place to
        // propagate a bad frame time.
        if t.is_nan() || t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        curve(self.solve_t_for_x(t), self.y1, self.y2)
    }

    /// Invert the curve's x component: given elapsed time, find the Bézier
    /// parameter. Newton–Raphson, with bisection as the fallback for the flat
    /// stretches where the derivative is near zero.
    fn solve_t_for_x(&self, x: f32) -> f32 {
        let mut t = x;
        for _ in 0..8 {
            let dx = curve(t, self.x1, self.x2) - x;
            if dx.abs() < 1e-6 {
                return t;
            }
            let d = slope(t, self.x1, self.x2);
            if d.abs() < 1e-6 {
                break;
            }
            t -= dx / d;
        }

        let (mut lo, mut hi, mut t) = (0.0f32, 1.0f32, x.clamp(0.0, 1.0));
        for _ in 0..24 {
            let v = curve(t, self.x1, self.x2);
            if (v - x).abs() < 1e-6 {
                break;
            }
            if v > x {
                hi = t;
            } else {
                lo = t;
            }
            t = (lo + hi) * 0.5;
        }
        t
    }
}

/// The 1-D cubic Bézier through 0, `a1`, `a2`, 1.
fn curve(t: f32, a1: f32, a2: f32) -> f32 {
    let u = 1.0 - t;
    3.0 * u * u * t * a1 + 3.0 * u * t * t * a2 + t * t * t
}

/// Derivative of [`curve`].
fn slope(t: f32, a1: f32, a2: f32) -> f32 {
    let u = 1.0 - t;
    3.0 * u * u * a1 + 6.0 * u * t * (a2 - a1) + 3.0 * t * t * (1.0 - a2)
}

/// Convert a per-16.67 ms smoothing coefficient into one for an arbitrary
/// frame time, so animation speed does not change with frame rate.
///
/// The mockups smooth with `cur += (tgt - cur) * k` once per browser frame.
/// That is only correct at 60 fps; this rewrites `k` for the frame we actually
/// got, which matters because the overlay loop drops to a 100 ms poll when idle.
#[must_use]
pub fn frame_rate_independent(k_at_60fps: f32, dt_ms: f32) -> f32 {
    let k = k_at_60fps.clamp(0.0, 1.0);
    if dt_ms <= 0.0 {
        return 0.0;
    }
    1.0 - (1.0 - k).powf(dt_ms / (1000.0 / 60.0))
}

/// One-pole smoothing factor for a time constant expressed in milliseconds.
#[must_use]
pub fn one_pole(tau_ms: f32, dt_ms: f32) -> f32 {
    if tau_ms <= 0.0 || dt_ms <= 0.0 {
        return 1.0;
    }
    1.0 - (-dt_ms / tau_ms).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_timings_are_what_the_report_specifies() {
        // If any of these change, the pill no longer matches the approved
        // mockup and the change needs a captain decision, not a commit.
        assert_eq!(ENTER_MS, 130);
        assert_eq!(EXIT_MS, 150);
        assert_eq!(STATE_CROSS_MS, 90);
        assert_eq!(CHECK_DRAW_MS, 240);
        assert_eq!(INSERTED_HOLD_MS, 550);
        // The report's shared budget: hidden -> listening <= 160 ms.
        const { assert!(ENTER_MS <= 160) };
        // ... and inserted hold in the 500-600 ms band.
        assert!(
            (500..=600).contains(&INSERTED_HOLD_MS),
            "{INSERTED_HOLD_MS}"
        );
    }

    #[test]
    fn easing_pins_its_endpoints() {
        for e in [EASE_IN, EASE_OUT] {
            assert_eq!(e.eval(0.0), 0.0);
            assert_eq!(e.eval(1.0), 1.0);
            assert_eq!(e.eval(-1.0), 0.0);
            assert_eq!(e.eval(2.0), 1.0);
            assert_eq!(e.eval(f32::NAN), 0.0);
        }
    }

    #[test]
    fn easing_is_monotonic() {
        for e in [EASE_IN, EASE_OUT] {
            let mut prev = -1.0;
            for i in 0..=200 {
                let v = e.eval(i as f32 / 200.0);
                assert!(v >= prev - 1e-4, "not monotonic at {i}: {v} < {prev}");
                assert!((-0.001..=1.001).contains(&v), "out of range at {i}: {v}");
                prev = v;
            }
        }
    }

    /// The whole point of `--in` is that it front-loads: the pill must look
    /// like it arrived long before the 130 ms is up.
    #[test]
    fn ease_in_is_front_loaded_and_ease_out_is_not() {
        assert!(EASE_IN.eval(0.25) > 0.6, "{}", EASE_IN.eval(0.25));
        assert!(EASE_OUT.eval(0.25) < 0.2, "{}", EASE_OUT.eval(0.25));
    }

    #[test]
    fn frame_rate_independent_smoothing_matches_at_60fps() {
        let k = frame_rate_independent(0.45, 1000.0 / 60.0);
        assert!((k - 0.45).abs() < 1e-4, "{k}");
        // A longer frame must move further, but never past the target.
        assert!(frame_rate_independent(0.45, 100.0) > 0.45);
        assert!(frame_rate_independent(0.45, 100.0) < 1.0);
        assert_eq!(frame_rate_independent(0.45, 0.0), 0.0);
    }

    #[test]
    fn one_pole_reaches_63_percent_at_one_tau() {
        let k = one_pole(25.0, 25.0);
        assert!((k - 0.632).abs() < 0.01, "{k}");
        assert_eq!(one_pole(0.0, 5.0), 1.0);
    }
}
