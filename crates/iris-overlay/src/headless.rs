//! Drive and rasterise the pill with no window, on any platform.
//!
//! Two jobs:
//!
//! 1. It is how the pill is tested. Every frame the Windows overlay puts on
//!    screen comes out of exactly this code, so asserting on pixels here is
//!    asserting on the real thing.
//! 2. It is how the pill is *reviewed* from WSL. `--filmstrip` in the demo
//!    walks a full state cycle and writes PNGs, which is a usable substitute
//!    for a Windows desktop when all you need is to check that the design
//!    landed.

use std::path::Path;

use crate::render::Renderer;
use crate::state::{Command, Model, OverlayState};
use crate::theme::Theme;
use crate::OverlayError;

/// One rendered frame: premultiplied RGBA, 8 bits per channel, no padding.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
    /// `width * height * 4` bytes, premultiplied RGBA, row-major, top-down.
    pub rgba: &'a [u8],
}

/// A pill with no window attached.
#[derive(Debug)]
pub struct HeadlessOverlay {
    model: Model,
    renderer: Renderer,
}

impl HeadlessOverlay {
    /// A fresh hidden pill at `scale` (`dpi / 96.0`).
    #[must_use]
    pub fn new(scale: f32, theme: Theme) -> Self {
        Self {
            model: Model::new(theme),
            renderer: Renderer::new(scale),
        }
    }

    /// Apply a command, exactly as the overlay thread would.
    ///
    /// Returns `false` for [`Command::Shutdown`].
    pub fn apply(&mut self, command: Command) -> bool {
        self.model.apply(command)
    }

    /// Advance to `now_ms` milliseconds since the pill started.
    pub fn tick(&mut self, now_ms: u64) {
        self.model.tick(now_ms);
    }

    /// Re-lay-out for a different monitor scale, as a `WM_DPICHANGED` would.
    pub fn set_scale(&mut self, scale: f32) {
        self.renderer.set_scale(scale);
    }

    /// The scale this pill is currently rendering at.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.renderer.layout().scale
    }

    /// Render the current model. Call after [`HeadlessOverlay::tick`].
    pub fn render(&mut self) {
        self.renderer.draw(&self.model);
    }

    /// The most recently rendered frame.
    #[must_use]
    pub fn frame(&self) -> Frame<'_> {
        let pixmap = self.renderer.pixmap();
        Frame {
            width: pixmap.width(),
            height: pixmap.height(),
            rgba: pixmap.data(),
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> OverlayState {
        self.model.state()
    }

    /// Whether the pill is off screen with nothing left to animate.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.model.is_idle()
    }

    /// The model, for tests that want to inspect the animation directly.
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Write the last rendered frame to a PNG.
    ///
    /// # Errors
    ///
    /// Returns [`OverlayError::Encode`] if the file cannot be written.
    pub fn write_png(&self, path: &Path) -> Result<(), OverlayError> {
        self.renderer
            .pixmap()
            .save_png(path)
            .map_err(|e| OverlayError::Encode(format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{EXIT_MS, INSERTED_HOLD_MS};
    use crate::theme::{PORCELAIN_LIGHT, PRISM_DARK};

    fn lit(frame: Frame<'_>) -> usize {
        frame.rgba.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    #[test]
    fn a_full_cycle_renders_every_state_and_ends_blank() {
        let mut pill = HeadlessOverlay::new(1.0, PRISM_DARK);
        pill.tick(0);
        pill.apply(Command::Engine("groq · whisper-large-v3-turbo · en".into()));
        pill.render();
        assert_eq!(lit(pill.frame()), 0, "hidden pill drew something");

        pill.apply(Command::ShowListening);
        let mut t = 0;
        let mut seen = vec![];
        let end = 1_200 + u64::from(INSERTED_HOLD_MS) + u64::from(EXIT_MS);
        // Thresholds, not equalities: the clock advances in 16 ms steps and
        // would step straight over an `== 600`.
        let (mut asked_processing, mut asked_inserted) = (false, false);
        while t < end {
            if t >= 800 && !asked_inserted {
                asked_inserted = true;
                pill.apply(Command::Inserted { latency_ms: 142 });
            } else if t >= 600 && !asked_processing {
                asked_processing = true;
                pill.apply(Command::Processing);
            } else {
                pill.apply(Command::Level(((t as f32) / 90.0).sin().abs()));
                pill.apply(Command::PartialLen((t / 20) as usize));
            }
            t += 16;
            pill.tick(t);
            pill.render();
            if !seen.contains(&pill.state()) {
                seen.push(pill.state());
            }
            if pill.state().is_visible() {
                assert!(lit(pill.frame()) > 1_000, "blank frame at {t} ms");
            }
        }

        assert_eq!(
            seen,
            vec![
                OverlayState::Listening,
                OverlayState::Processing,
                OverlayState::Inserted,
                OverlayState::Hidden
            ]
        );
        assert!(pill.is_idle());
        assert_eq!(lit(pill.frame()), 0, "pill did not clean up after itself");
    }

    #[test]
    fn the_frame_buffer_is_the_size_it_claims() {
        for scale in [1.0, 1.5, 2.0] {
            let mut pill = HeadlessOverlay::new(scale, PORCELAIN_LIGHT);
            pill.tick(0);
            pill.apply(Command::ShowListening);
            pill.tick(200);
            pill.render();
            let f = pill.frame();
            assert_eq!(
                f.rgba.len(),
                (f.width * f.height * 4) as usize,
                "scale {scale}"
            );
        }
    }

    #[test]
    fn rendering_without_ticking_first_does_not_panic() {
        let mut pill = HeadlessOverlay::new(1.0, PRISM_DARK);
        pill.render();
        assert_eq!(lit(pill.frame()), 0);
    }

    #[test]
    fn writing_a_png_produces_a_real_file() {
        let mut pill = HeadlessOverlay::new(1.0, PRISM_DARK);
        pill.tick(0);
        pill.apply(Command::ShowListening);
        pill.apply(Command::Level(0.8));
        pill.tick(300);
        pill.render();

        let mut path = std::env::temp_dir();
        path.push(format!("iris-overlay-test-{}.png", std::process::id()));
        pill.write_png(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(bytes.len() > 1_000, "png is {} bytes", bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a png");
    }

    #[test]
    fn writing_to_an_impossible_path_is_an_error_not_a_panic() {
        let mut pill = HeadlessOverlay::new(1.0, PRISM_DARK);
        pill.tick(0);
        pill.render();
        let err = pill.write_png(Path::new("/definitely/not/a/directory/x.png"));
        assert!(matches!(err, Err(OverlayError::Encode(_))));
    }
}
