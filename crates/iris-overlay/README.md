# iris-overlay

The Iris pill: a small always-on-top capsule that appears bottom-centre while
you hold the dictation hotkey, shows the microphone as a 28-bar spectrum
waveform, confirms the insert with a latency print, and takes itself off screen.

It is the product's hero surface, and it is deliberately dumb: it never takes
focus, never accepts a click, never holds transcript text, and **never types**.
Text injection lives in `iris-core` and is not reachable from here.

```
┌──────────────────────────────────────────────┐
│  ◉   ▁▃▅█▇▅▃▁▂▄▆█▇▅▃▁▂▄▆█▇▅▃▁▂        0:03  │   listening
└──────────────────────────────────────────────┘
        groq · whisper-large-v3-turbo · en
```

## Using it

```rust
use iris_overlay::{spawn, OverlayConfig};

let overlay = spawn(OverlayConfig {
    engine: "groq · whisper-large-v3-turbo · en".into(),
    ..Default::default()                 // Prism dark
})?;
let pill = overlay.handle();             // Clone + Send

pill.show_listening();                   // hotkey down
pill.update_level(0.62);                 // per audio frame
pill.set_partial_len(31);                // per partial transcript
pill.processing();                       // hotkey up
pill.inserted(142);                      // text landed; hides itself
```

`OverlayHandle` is the entire contract and it is intended to stay stable. Every
method is non-blocking and infallible from the caller's side: if the overlay
thread is gone or its queue is momentarily full the command is dropped, because
a display that back-pressures the audio pipeline is worse than one that skips a
frame. `hide()` cancels from any state; `set_theme()` swaps palettes at runtime.

Two transitions are worth knowing about:

- `inserted()` may be called straight from `listening` — a streaming engine that
  has already finalised does not have to fake a processing phase.
- `processing()` and `inserted()` from `hidden` are **ignored**. A late engine
  event cannot flash a pill back onto a screen the user already dismissed.

The full transition table is on `Model::apply`.

## Design provenance

Implements the captain-locked design (2026-07-31):

| Decision | Here |
|---|---|
| Prism = v1 dark default | `theme::PRISM_DARK` |
| Porcelain = day-one light theme | `theme::PORCELAIN_LIGHT` |
| Listening-only telemetry chip; latency on inserted | `OverlayState::shows_chip`, `Model::readout` |
| Prism-triangle app icon | `assets/iris-prism.svg`, re-exported as `APP_ICON_SVG` |

Geometry and motion are **single-sourced from Prism** and a `Theme` carries
colour and nothing else, so switching skins cannot move a pixel. That is the
literal reading of the captain's "same geometry, swapped tokens".

The numbers in `motion.rs` and `layout.rs` are acceptance criteria, not
preferences, and each has a test that pins it:

```
188 × 34 px, radius 17, bottom-centre, 58 px above the work area
enter 130 ms · exit 150 ms · state cross 90 ms · check draw 240 ms
inserted hold 550 ms
28 bars × scaleY only · no blur filter · no animated gradient stops
```

Desk feedback tightened the body from the original Prism mockup bar
(`248 × 46`) so it reads as a small HUD chip rather than a digital recorder
strip. Placement, spectrum-on-waveform-only, listening-only chip, and Prism /
Porcelain token locks are unchanged.

Two places where the implementation deliberately departs from the mockup, both
because the mockup is a web page and this is not:

- **Inserted hold is 550 ms.** Prism's spec card says 500 and Porcelain's says
  600; the report's shared constraint is "~500–600 ms" and the brief locked the
  midpoint.
- **The processing scan sweeps the track once per period.** The mockup
  translates the band by 280 % of its own width, which throws it clean off the
  pill for more than half of each cycle — the mock has no `overflow: hidden`, so
  nothing stops it. Here the signal layer is clipped to the pill and the travel
  is sized so the band enters and leaves once per 720 ms.

One element has no mockup counterpart: the **partial ribbon**, a thin spectrum
underline beneath the waveform that fills as the partial transcript grows. The
locked API includes `set_partial_len` and the mockups have nothing driven by it,
so it needed a home. A `scaleX`-only underline derived from the existing top
hairline is the smallest thing that obeys the spectrum rules and does not thrash
layout. It carries a *length*, never text — the overlay holds no transcript
content, so there is nothing to read over the user's shoulder and nothing in a
crash dump.

## Why a CPU raster path

The pill is ~250 × 100 device pixels at 100 %, ~500 × 200 at 200 %. It is
rasterised on the CPU with [tiny-skia] and blitted with `UpdateLayeredWindow`.

A GPU surface was considered and rejected: at this size the entire frame is
under 60 K pixels, which a single core rasterises in well under a millisecond,
while a D3D/D2D path would add adapter enumeration, device-lost handling, a
swapchain that has to cooperate with `WS_EX_LAYERED`, and a second code path
that cannot be tested anywhere but a real Windows desktop. A WebView2 pill was
never on the table — a browser process for a 34 px capsule is exactly the
"heavy" the report rules out, and it cannot be made click-through and
non-activating without fighting it.

What the CPU path buys, beyond simplicity:

- **The renderer is portable.** `render/` has no Windows in it, so the exact
  frames the overlay shows can be produced, diffed and eyeballed on Linux. The
  pixel assertions in this crate's tests are assertions about the real thing.
- **It cross-compiles from WSL with nothing but mingw.** tiny-skia and fontdue
  are pure Rust. No cmake, no Windows SDK, no shader compiler. See
  `docs/dev-windows.md` for why that matters to this repository.
- **Zero cost when hidden.** The loop parks on the command channel and the
  window is hidden; there is no swapchain to keep alive and no compositor
  callback to service.

Blur is the one thing a CPU path has to earn. The drop shadow and the coloured
state halo are three-pass box blurs of an 8-bit mask (`render/shadow.rs`),
cached and only recomputed while the pill is actually moving. This is not a
backdrop filter and does not read the desktop behind the window — the report
forbids that, and `UpdateLayeredWindow` could not do it anyway.

### Text

The timer, the latency figure and the engine chip are all mono in the Prism
mockup, so the pill needs exactly one face. Cascadia Mono — the font the design
spec names — is SIL OFL 1.1, so it ships in `assets/fonts/` and is rasterised
with [fontdue] rather than resolved from the system font stack. That keeps the
Windows and Linux renders byte-identical, which is what makes a PNG a usable
review artefact.

[tiny-skia]: https://github.com/RazrFalcon/tiny-skia
[fontdue]: https://github.com/mooman219/fontdue

## The window

`window/win32.rs`, and it is the only non-portable file.

| Requirement (design report checklist) | How |
|---|---|
| Per-pixel alpha | `WS_EX_LAYERED` + `UpdateLayeredWindow(..., ULW_ALPHA)` |
| Click-through | `WS_EX_TRANSPARENT`, plus `HTTRANSPARENT` from `WM_NCHITTEST` |
| Never activates | `WS_EX_NOACTIVATE`, `SW_SHOWNOACTIVATE`, `SWP_NOACTIVATE` |
| Out of Alt-Tab | `WS_EX_TOOLWINDOW` |
| Always on top | `WS_EX_TOPMOST` |
| Per-monitor V2 DPI | `SetThreadDpiAwarenessContext` on the overlay thread only |

Two details worth calling out:

**DPI awareness is set per-thread, not per-process.** This is a library. The
host process may have no manifest or a different awareness level, and changing
that on its behalf would be rude and would break its own windows. On
`WM_DPICHANGED` the layout is rebuilt at the monitor's real scale and the pill
re-rasterised — nothing is drawn at 96 dpi and stretched.

**The pill follows the foreground window's monitor.** On a multi-monitor desk
it appears under the app you are dictating into, not always on the primary.

## Running the demo

```bash
# The real pill. Windows only; from WSL, build and run the exe (see below).
cargo run --example pill-demo
cargo run --example pill-demo -- --theme porcelain --cycles 0   # until Ctrl-C

# A PNG filmstrip of the same frames. Works anywhere, including Linux CI.
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill --scale 1.5
```

The demo drives a full cycle with a synthetic speech envelope — syllables riding
on a phrase-length swell, transcribed from the mockup's own `envelope()` — so it
looks like someone talking rather than a test tone.

### From WSL

`docs/dev-windows.md` has the full toolchain story; the overlay-specific loop is:

```bash
# Portable: state machine, layout, tokens, and the rasteriser itself.
cargo test -p iris-overlay

# See it, without a Windows desktop.
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill

# Type-check and build the Windows window layer.
cargo check -p iris-overlay --target x86_64-pc-windows-gnu
cargo build --release --example pill-demo --target x86_64-pc-windows-gnu

# Run it as a real Windows process, straight from the WSL prompt.
./target/x86_64-pc-windows-gnu/release/examples/pill-demo.exe
```

That last line is the point: WSL interop launches the `.exe` as a genuine
Windows process, so the pill appears on the actual desktop with real DPI, a real
work area and real z-order. Run it by path — `cmd.exe` cannot use a
`\\wsl.localhost\...` working directory.

## Layout of the crate

| File | What |
|---|---|
| `theme.rs` | Colour tokens. Two `const` palettes, and nothing but colour. |
| `motion.rs` | Timing constants and the two cubic-bezier curves. |
| `layout.rs` | Logical geometry, DPI scaling, window placement. |
| `spectrum.rs` | The 28-bar shape function. |
| `state.rs` | States, commands, and the animated model. No clock, no window. |
| `render/` | tiny-skia rasteriser. Portable. |
| `window/win32.rs` | The layered window. The only `cfg(windows)` file. |
| `window/stub.rs` | The same loop with no window, everywhere else. |
| `handle.rs` | `spawn`, `Overlay`, `OverlayHandle`. |
| `headless.rs` | Drive and rasterise the pill with no window, anywhere. |

## Licence

The crate is MIT, like the rest of Iris. `assets/fonts/CascadiaMono-Regular.ttf`
is Copyright Microsoft Corporation under SIL OFL 1.1; the licence text ships
alongside it in `assets/fonts/OFL.txt`.
