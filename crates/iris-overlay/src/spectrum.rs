//! The 28-bar waveform model.
//!
//! Bars are animated with `scaleY` and nothing else — no width, no position, no
//! colour animation. Each bar's colour is a fixed sample of the theme's ramp,
//! so the spectrum is a *static* gradient across the pill that the signal
//! modulates vertically. That is the Prism rule the report is explicit about:
//! "bars always ramped, never animate gradient stops".
//!
//! The shape function is the mockup's, transcribed: a `sin(pi*p)^0.5` taper so
//! the ends of the row stay short, times a two-frequency wobble so neighbouring
//! bars never move in lockstep, times the microphone envelope.

use crate::motion::frame_rate_independent;

/// Number of waveform bars. Prism's signature count — the report lists it as an
/// implementation spec (`28 bars x scaleY()`), not a suggestion.
pub const BAR_COUNT: usize = 28;

/// The `scaleY` a bar sits at with no signal at all. The row never fully
/// collapses: a flat line reads as "broken", a shallow ripple reads as "armed".
pub const IDLE_FLOOR: f32 = 0.07;

/// Envelope used while processing — a low shimmer under the scan band.
pub const PROCESSING_ENV: f32 = 0.13;

/// Envelope used when the pill is visible but not listening.
pub const RESTING_ENV: f32 = 0.055;

/// Per-frame approach rate (at 60 fps) while listening. Fast, because the bars
/// are the only thing on screen telling the user the mic is live.
const K_LISTENING: f32 = 0.45;

/// Per-frame approach rate (at 60 fps) otherwise. Slow, so state changes settle
/// rather than snap.
const K_RESTING: f32 = 0.15;

/// The `scaleY` bar `index` is heading toward, for envelope `env` at time
/// `now_ms`.
///
/// `env` is the smoothed microphone level in 0.0–1.0.
#[must_use]
pub fn bar_target(index: usize, env: f32, now_ms: u64) -> f32 {
    debug_assert!(index < BAR_COUNT);
    let t = now_ms as f32;
    let p = index as f32 / (BAR_COUNT - 1) as f32;
    let i = index as f32;

    // Ends of the row stay short so the waveform reads as a lens, not a bar
    // chart. sqrt of a sine lobe: broad in the middle, quick taper at the ends.
    let taper = (std::f32::consts::PI * p).sin().max(0.0).sqrt();

    // Two incommensurate frequencies, offset per bar. Cheap, deterministic,
    // and never lines up into a visible travelling wave.
    let wobble = 0.4 + 0.6 * ((t / 88.0 + i * 0.9).sin() * (t / 230.0 + i * 0.33).sin()).abs();

    // The mockup writes `0.07 + env*taper*n*0.95`, which can reach scaleY 1.02
    // and clip against the top of the waveform box. Letting the signal fill
    // exactly what the floor leaves over keeps the peak at 1.0.
    IDLE_FLOOR + env.clamp(0.0, 1.0) * taper * wobble * (1.0 - IDLE_FLOOR)
}

/// Advance every bar one frame toward its target.
///
/// `responsive` selects the fast approach rate; it is true while listening.
/// `dt_ms` makes the smoothing frame-rate independent, which matters because
/// the window loop runs at 60 fps when visible and 10 Hz when idle.
pub fn advance(bars: &mut [f32; BAR_COUNT], env: f32, now_ms: u64, dt_ms: f32, responsive: bool) {
    let k = frame_rate_independent(if responsive { K_LISTENING } else { K_RESTING }, dt_ms);
    for (i, bar) in bars.iter_mut().enumerate() {
        let target = bar_target(i, env, now_ms);
        *bar += (target - *bar) * k;
    }
}

/// A row of bars at rest, for initialising a fresh model.
#[must_use]
pub fn resting_bars() -> [f32; BAR_COUNT] {
    [IDLE_FLOOR; BAR_COUNT]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_count_is_the_prism_spec() {
        assert_eq!(BAR_COUNT, 28);
    }

    #[test]
    fn silence_still_leaves_a_floor() {
        for i in 0..BAR_COUNT {
            let h = bar_target(i, 0.0, 12_345);
            assert!((h - IDLE_FLOOR).abs() < 1e-6, "bar {i} = {h}");
        }
    }

    #[test]
    fn targets_stay_inside_the_bar_box() {
        // scaleY > 1 would clip against the pill; the shape function must not
        // be able to produce it at full signal.
        for t in (0..5_000).step_by(7) {
            for i in 0..BAR_COUNT {
                let h = bar_target(i, 1.0, t);
                assert!((IDLE_FLOOR..=1.0).contains(&h), "t={t} bar={i} h={h}");
            }
        }
    }

    #[test]
    fn level_is_clamped_not_trusted() {
        // An engine handing us a level of 4.0 must not blow the bars past the
        // pill; an engine handing us -1.0 must not invert them.
        for i in 0..BAR_COUNT {
            assert!(bar_target(i, 9.0, 100) <= 1.0);
            assert!(bar_target(i, -9.0, 100) >= IDLE_FLOOR - 1e-6);
        }
    }

    #[test]
    fn the_row_is_tapered() {
        // Middle bars must be able to out-reach the end bars; otherwise the
        // waveform is a rectangle.
        let mid = bar_target(BAR_COUNT / 2, 1.0, 0);
        let end = bar_target(0, 1.0, 0);
        assert!(mid > end, "mid {mid} should exceed end {end}");
        assert!((bar_target(0, 1.0, 999) - IDLE_FLOOR).abs() < 1e-6);
        assert!((bar_target(BAR_COUNT - 1, 1.0, 999) - IDLE_FLOOR).abs() < 1e-6);
    }

    #[test]
    fn neighbours_do_not_move_in_lockstep() {
        // If adjacent bars were identical the row would look like one block.
        let a: Vec<f32> = (0..BAR_COUNT).map(|i| bar_target(i, 1.0, 700)).collect();
        let distinct = a.windows(2).filter(|w| (w[0] - w[1]).abs() > 1e-3).count();
        assert!(
            distinct > BAR_COUNT / 2,
            "only {distinct} distinct neighbours"
        );
    }

    #[test]
    fn advance_converges_toward_the_target() {
        let mut bars = resting_bars();
        // A steady loud signal, sampled at a frozen instant so the target is
        // constant: 20 frames must get essentially all the way there.
        for _ in 0..20 {
            advance(&mut bars, 1.0, 500, 1000.0 / 60.0, true);
        }
        for (i, bar) in bars.iter().enumerate() {
            let want = bar_target(i, 1.0, 500);
            assert!((bar - want).abs() < 0.01, "bar {i}: {bar} vs {want}");
        }
    }

    #[test]
    fn advance_is_frame_rate_independent() {
        // One 100 ms frame must land close to six 16.67 ms frames.
        let mut slow = resting_bars();
        advance(&mut slow, 1.0, 500, 100.0, true);

        let mut fast = resting_bars();
        for _ in 0..6 {
            advance(&mut fast, 1.0, 500, 100.0 / 6.0, true);
        }

        for i in 0..BAR_COUNT {
            assert!(
                (slow[i] - fast[i]).abs() < 0.02,
                "bar {i}: {} vs {}",
                slow[i],
                fast[i]
            );
        }
    }

    #[test]
    fn advance_never_produces_a_non_finite_height() {
        let mut bars = resting_bars();
        for step in 0..300 {
            advance(&mut bars, f32::NAN.max(0.5), step * 17, 17.0, step % 3 == 0);
            assert!(bars.iter().all(|b| b.is_finite()));
        }
    }
}
