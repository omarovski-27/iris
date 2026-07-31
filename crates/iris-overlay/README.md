# iris-overlay

The Iris pill: a small always-on-top shape that appears bottom-centre while
you hold the dictation hotkey. A quiet orb while it waits for speech, opening
into a capsule that shows the live transcript as words arrive, then
collapsing back into a checkmark the instant text lands — and taking itself
off screen a moment later.

It is the product's hero surface. It never takes focus, never accepts a
click, and **never types**: text injection lives in `iris-core` and is not
reachable from here.

```
listening, quiet          listening, words arriving              inserted
     ⬤              ─────▶   ╭──────────────────────────╮   ─────▶    ⓥ
                              │  ...the report needs three │           134 ms
                              ╰──────────────────────────╯
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
pill.set_partial_text("the quarterly");  // per partial transcript
pill.processing();                       // hotkey up
pill.inserted(142);                      // text landed; hides itself
```

`OverlayHandle` is the contract. Every method is non-blocking and infallible
from the caller's side: if the overlay thread is gone or its queue is
momentarily full the command is dropped, because a display that
back-pressures the audio pipeline is worse than one that skips a frame.
`hide()` cancels from any state; `set_theme()` swaps palettes at runtime.

Two transitions are worth knowing about:

- `inserted()` may be called straight from `listening` — a streaming engine that
  has already finalised does not have to fake a processing phase.
- `processing()` and `inserted()` from `hidden` are **ignored**. A late engine
  event cannot flash a pill back onto a screen the user already dismissed.

The full transition table is on `Model::apply`.

## The contract changed, and here is why

Two things used to be true of this crate, stated plainly in this file and in
`CLAUDE.md`: the API was "intended to stay stable", and the overlay "never
holds transcript text… nothing to read over the user's shoulder and nothing
in a crash dump." Both changed with this design.

**Why.** The previous design — a fixed 168×34 capsule with a 28-bar spectrum
waveform — was signed off, then rejected on sight the same day: *"I don't
like the UI, we need to change it, transform it completely, but I like the
motions and smoothness of it."* Three redesigns were rendered for real
(`data/iris-ui-directions/report.md` in the fleet's records is the design
history, kept as the record of *why*, not duplicated here); the captain chose
this one — an orb that opens into a live-text ribbon — specifically *because*
it shows the words. Shipping the shape without the text would have been a
different, weaker direction than the one that was actually chosen.

**What changed, concretely:**

- `OverlayHandle::set_partial_len(usize)` is gone. `OverlayHandle::set_partial_text(impl Into<String>)`
  replaces it: the overlay now holds the live transcript for exactly as long
  as it is on screen.
- `Model` gained a `text: String` field. Nothing else on the contract moved —
  `show_listening`, `update_level`, `set_engine`, `processing`, `inserted`,
  `hide`, `set_theme` are all unchanged, so this is additive-with-one-removal,
  not a rewrite.
- This authorises **displaying** text on screen. It does not authorise
  persisting, transmitting, or logging it anywhere — nothing in this crate
  writes the transcript to disk, and that stays true.
- **Ship it with an opt-out.** `iris-app` gates `set_partial_text` behind a
  config setting (default on) that, when off, leaves the orb-only
  presentation running with no ribbon and no text ever reaching this crate —
  a complete, coherent design on its own, not a degraded fallback. See
  `iris-app/src/config.rs` and `pill.rs`.

If you are extending this crate: the bar for adding to `OverlayHandle` is
still "the smallest honest change", the same as before. This one addition
earned its way in because the alternative was silently shipping a narrower
feature than the one that was actually approved.

## The shape

A capsule whose corner radius is always exactly half its height. At minimum
width that makes it a true circle — the orb — with no visible seam; at any
wider width it is a capsule holding the live transcript. There is one shape,
not two: only its width animates. Height, placement, and every motion timing
are unchanged from before.

- `layout::ORB_D` (34) is both the orb's diameter and the ribbon's constant
  height — deliberately equal to the previous design's `PILL_H`, both as
  continuity and because it is what makes the morph seamless.
- `layout::RIBBON_MAX_W` (460) is the widest the ribbon grows before new words
  start scrolling the oldest ones off the left edge (`render::text::trailing_fit`)
  instead of growing further.
- The window is **fixed-size**, sized for the widest state up front
  (`layout::WINDOW_W` / `WINDOW_H`). Only the shape drawn inside it animates —
  see `window/win32.rs`'s `Surface::present`, which already hands a fresh size
  to `UpdateLayeredWindow` every frame regardless of whether it changed. This
  was the lower-risk of two options (the other being a window that resizes
  live), and the fixed transparent margin around a narrow orb costs nothing
  extra to composite.
- `layout::WORK_AREA_GAP` (58) is unchanged from the previous pill on purpose:
  this direction changes the shape, not where the eye has to look for it.

## Design provenance

The captain's decision, recorded 2026-07-31: orb → live-text ribbon, "make it
exceptionally beautiful", live text on by default with a config opt-out. It
supersedes the earlier captain-locked pill geometry (168×34 fixed capsule,
28-bar spectrum, listening-only telemetry chip) recorded in `CLAUDE.md`'s
history — that geometry is gone from this crate; the Prism/Porcelain palettes
and the motion budget are not.

**What carried over intact, and what did not.**

| | Then | Now |
|---|---|---|
| Geometry | Fixed 168×34 capsule | One shape, width animates 34→460 |
| Motion | `motion.rs` timings and curves | **Identical** — every constant is imported, none copied |
| Colour | Prism dark / Porcelain light | Same two palettes, same tokens, no new colours needed |
| Waveform | 28-bar spectrum (`spectrum.rs`) | A new, independently-tuned bar row in `render/mod.rs`'s `draw_wave` — see "Glass, and the wave came back", below. `spectrum.rs` itself is gone; nothing shares code with it. |
| Shell | Opaque | Translucent glass, boosted dynamically for legibility while text shows |
| Transcript | Never held (`set_partial_len`, a count) | Held while on screen (`set_partial_text`, the string) |
| Engine chip | Rendered below the pill | Carried on the model, not rendered — no room without competing with the words |

Geometry and motion are **still single-sourced** and a `Theme` is still
colour and nothing else — swapping `PRISM_DARK` for `PORCELAIN_LIGHT` moves
zero pixels, the same guarantee the previous design made, verified the same
way: `cargo run --example pill-demo -- --theme porcelain --filmstrip <dir>`
and `--theme prism` are the identical code path.

One idea has no counterpart in the previous design and is worth naming
because it very nearly shipped as a bug: the width morph is smoothed with a
one-pole filter (the same attack/release character the microphone level meter
already used), and one-pole smoothing is asymptotic — it approaches its
target but never exactly reaches it. A ribbon that looked fully grown but was
a sub-pixel short of fitting its text used to silently drop a leading
character. `render::mod.rs`'s `Renderer::draw` snaps the smoothed width to its
exact target once within ~1.5 px specifically to close that gap; there is a
regression test for it (`width_smoothing_does_not_drop_a_whole_character_once_settled`)
and it must not be "simplified" away.

## Glass, and the wave came back

A first pass of this shell shipped nearly opaque and dropped the 28-bar
waveform for a plain pulsing dot. Direct captain feedback after living with
it on a real desktop: *"I think if we make it glassy... because it's now just
black. And I like the waves... maybe just improve the waves more when the
volume is higher, so it's showing that it's clearly hearing you."* Both were
addressed as refinements within this same direction, not a new one.

**Glass.** `theme::PRISM_DARK` / `PORCELAIN_LIGHT`'s `shell_top` and
`shell_bottom` now carry alpha well under 1.0 — real translucency, not a
faked effect, because the overlay is already a per-pixel-alpha layered window
and this is the same compositing `UpdateLayeredWindow` was already doing
every frame. What this is **not**: acrylic/Mica-style backdrop blur. A
layered window does not get a read of what is behind it for free, and this
crate does not fake one — no sampling, no guessed blur. The glass
*impression* instead comes from three honest ingredients, all in
`render/mod.rs`'s `draw_shell`: the translucent fill itself, a soft
`glass_sheen` wash brighter at the top (light catching a curved surface), and
the existing rim (`outer_ring`/`border`) plus the crisp `inner_highlight`
line. Because live text has to stay legible over an arbitrary desktop, the
fill's alpha is boosted smoothly as the ribbon opens — reusing `text_alpha`,
the same curve that fades the words in — so legibility firms up exactly when
there is text that needs it, and the shell stays at its most transparent at
rest.

**The wave.** `draw_wave` is a new, independently-tuned bar row, not a port
of the deleted `spectrum.rs`. Two things are deliberately different, both to
fix a problem the old one had:

- *A taper floor, not a taper to zero.* The old row's `sqrt(sin(π·p))` taper
  hit exactly zero at both ends and stayed under 75% of peak height for the
  outer ~18% of bars each side even at full volume — see the design report
  for the exact numbers computed from that formula. That is what a captain
  live-desktop observation named independently: *"the waves... get cut off
  about 75%."* This row's taper has a floor of 0.4, so it keeps the same
  gentle lens shape without ever going fully flat.
- *An expansive response curve.* Level is raised to `powf(1.6)` before it
  drives bar height, which widens the visible gap between quiet and loud
  instead of compressing it — quiet reads clearly quiet, loud reads clearly
  loud, "so it's showing that it's clearly hearing you" in the captain's own
  words, which is a real functional need and not only an aesthetic one.

Bar count and pitch are recomputed from the shape's *current* width every
frame rather than fixed, so the row is never sparse-and-thin at the wide-open
ribbon or crowded-past-legibility at the 34px orb — a direct application of
the fill-width lesson the old row's bug taught. It sits in a band above the
shape's centre, coexisting with the text and the core glyph rather than
replacing either.

## Why a CPU raster path

The pill is small — even at its widest (the open ribbon) the whole frame is a
few hundred thousand device pixels at 200% scale. It is rasterised on the CPU
with [tiny-skia] and blitted with `UpdateLayeredWindow`.

A GPU surface was considered and rejected: at this size a D3D/D2D path would
add adapter enumeration, device-lost handling, a swapchain that has to
cooperate with `WS_EX_LAYERED`, and a second code path that cannot be tested
anywhere but a real Windows desktop. A WebView2 pill was never on the table —
a browser process for a small HUD shape is exactly the "heavy" this design
rules out, and it cannot be made click-through and non-activating without
fighting it.

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
state halo are three-pass box blurs of an 8-bit mask (`render/shadow.rs`).
Unlike the previous fixed-width pill, these masks **cannot be cached
forever** — the shape's width changes on nearly every frame while the ribbon
is opening, closing, or the transcript is growing, so the cache key in
`ensure_masks` includes the current width, rounded to the nearest 4 device
px, so a settled ribbon still hits the cache between frames and only the
brief morph window pays full cost. This is not a backdrop filter and does not
read the desktop behind the window — the report forbids that, and
`UpdateLayeredWindow` could not do it anyway.

### Text

Cascadia Mono — the font the design spec names — is SIL OFL 1.1, so it ships
in `assets/fonts/` and is rasterised with [fontdue] rather than resolved from
the system font stack. That keeps the Windows and Linux renders
byte-identical, which is what makes a PNG a usable review artefact.

`render::text::FontAtlas` has no clip-mask parameter, so the live transcript's
overflow handling is a string trim, not a pixel clip: when the transcript is
wider than the ribbon's padded interior, `trailing_fit` finds the longest
*tail* that fits and it is drawn right-aligned. The newest word always sits
against the right padding; the oldest ones quietly drop off the left. No new
text-rendering primitive was needed for this.

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

This holds even though the overlay now shows text: nothing in this design
changes how the window handles input, because nothing in this design needed
to. The shape shows what was heard; it still never listens for a click.

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
cargo run --example pill-demo -- --theme porcelain --utterance short --cycles 0   # until Ctrl-C

# A PNG filmstrip of the same frames. Works anywhere, including Linux CI.
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill --utterance long --scale 1.5
```

The demo drives a full cycle with a synthetic speech envelope — syllables
riding on a phrase-length swell — and a scripted utterance revealed one word
at a time (`--utterance short` fits comfortably; `--utterance long`, the
default, overflows the ribbon on purpose so the marquee-tail scroll is easy to
review).

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
| `state.rs` | States, commands, and the animated model. No clock, no window, no shape. |
| `render/` | tiny-skia rasteriser, including the width-morph tween. Portable. |
| `window/win32.rs` | The layered window. The only `cfg(windows)` file. |
| `window/stub.rs` | The same loop with no window, everywhere else. |
| `handle.rs` | `spawn`, `Overlay`, `OverlayHandle`. |
| `headless.rs` | Drive and rasterise the pill with no window, anywhere. |

## Licence

The crate is MIT, like the rest of Iris. `assets/fonts/CascadiaMono-Regular.ttf`
is Copyright Microsoft Corporation under SIL OFL 1.1; the licence text ships
alongside it in `assets/fonts/OFL.txt`.
