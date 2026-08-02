//! The Iris pill overlay — the hero UI surface.
//!
//! A small always-on-top shape that appears bottom-centre while you hold the
//! dictation hotkey: by default a quiet glass capsule holding a wave row and
//! an elapsed-recording timer, widening into a live-transcript ribbon only
//! when the consumer opts in (`iris-app`'s `show_live_text`, off by default),
//! then collapsing back to a checkmark on insert before it takes itself off
//! screen. It never takes focus, never accepts a click, and never types: text
//! injection lives elsewhere in Iris and deliberately not here.
//!
//! # Using it
//!
//! ```no_run
//! use iris_overlay::{spawn, OverlayConfig, PORCELAIN_LIGHT};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let overlay = spawn(OverlayConfig {
//!     engine: "groq · whisper-large-v3-turbo · en".into(),
//!     ..Default::default()          // Prism dark
//! })?;
//! let pill = overlay.handle();      // Send + Clone: drive it from any thread
//!
//! pill.show_listening();
//! pill.update_level(0.7);
//! pill.set_partial_text("the quarterly report needs");
//! pill.processing();
//! pill.inserted(142);               // hides itself ~550 ms later
//!
//! pill.set_theme(PORCELAIN_LIGHT);  // light skin, same geometry
//! # Ok(())
//! # }
//! ```
//!
//! [`OverlayHandle`] is the whole contract. Everything else in this crate is
//! either the data behind the design tokens ([`theme`]), the acceptance
//! criteria the motion is held to ([`motion`], [`layout`]), or testing
//! scaffolding ([`HeadlessOverlay`]).
//!
//! # Design provenance
//!
//! This direction — one shape whose width animates, up to a live-text
//! ribbon — replaced an earlier fixed-size capsule design; see `README.md`
//! "Design provenance" for the full record and why the overlay may now hold
//! transcript text when it previously never did, and "Round 3" for why that
//! ribbon is opt-in.
//! Geometry and motion stay single-sourced and a theme swaps colours only, so
//! switching skins cannot move a pixel — that discipline carried over from the
//! previous design intact. The numbers in [`motion`] and [`layout`] are
//! acceptance criteria, not preferences — see `README.md`.
//!
//! # Platforms
//!
//! The window is Windows-only. On other platforms [`spawn`] still starts the
//! same state machine with no window, so a consumer builds and runs on Linux;
//! to see the pixels there, use [`HeadlessOverlay`] or the demo's `--filmstrip`
//! mode.

#![warn(missing_docs)]

mod handle;
mod headless;
pub mod layout;
pub mod motion;
mod render;
pub mod state;
pub mod theme;
mod window;

pub use handle::{spawn, Overlay, OverlayConfig, OverlayHandle};
pub use headless::{Frame, HeadlessOverlay};
pub use layout::{Layout, Placement, WorkArea};
pub use state::{Command, OverlayState};
pub use theme::{Rgba, Theme, PORCELAIN_LIGHT, PRISM_DARK, THEMES};

/// The app icon: a prism triangle, light in and spectrum out.
///
/// The captain's locked mark. Shipped as source so a packaging step can render
/// it to `.ico` at whatever sizes it needs; the crate itself never draws it.
pub const APP_ICON_SVG: &str = include_str!("../assets/iris-prism.svg");

/// Anything that can go wrong starting or running the overlay.
#[derive(Debug)]
pub enum OverlayError {
    /// The platform refused to give us a window or a surface. Carries the
    /// underlying Win32 error text.
    Platform(String),
    /// A frame could not be written to disk.
    Encode(String),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::Platform(m) => write!(f, "overlay window error: {m}"),
            OverlayError::Encode(m) => write!(f, "overlay frame encode error: {m}"),
        }
    }
}

impl std::error::Error for OverlayError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_icon_is_a_usable_svg() {
        assert!(APP_ICON_SVG.contains("<svg"), "icon is not svg");
        assert!(APP_ICON_SVG.contains("viewBox"), "icon does not scale");
        assert!(APP_ICON_SVG.contains("</svg>"), "icon is truncated");
        // Self-contained: an icon that fetches something is not an icon.
        // (The `xmlns` URI is a namespace name, not a fetch.)
        assert!(!APP_ICON_SVG.contains("href"), "icon links to something");
        assert!(!APP_ICON_SVG.contains("<image"), "icon embeds a raster");
        assert!(
            !APP_ICON_SVG.contains("url(http"),
            "icon fetches a paint server"
        );
    }

    #[test]
    fn errors_say_something_useful() {
        let e = OverlayError::Platform("CreateWindowExW failed".into());
        assert!(e.to_string().contains("CreateWindowExW"));
        let e: Box<dyn std::error::Error> = Box::new(OverlayError::Encode("disk full".into()));
        assert!(e.to_string().contains("disk full"));
    }
}
