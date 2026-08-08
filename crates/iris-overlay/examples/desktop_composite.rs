//! Composite an already-rendered overlay PNG onto a large neutral desktop
//! canvas at true, unscaled device-pixel size.
//!
//! Every evidence frame this crate renders (`pill-demo --filmstrip` /
//! `--evidence`) is a tight crop around the shape, at whatever pixel
//! dimensions the shape itself is — commonly a few hundred px wide. Opened
//! directly, a docs viewer or image tool routinely scales a file that small
//! up to fill its window, which silently zooms in on exactly the kind of
//! detail (a couple of device px of bar height, a low-alpha fill) that
//! stops being legible at real desktop scale. This tool exists to remove
//! that step: it places the source frame, unscaled, in the middle of a much
//! larger canvas, so a viewer's own upscaling of the *whole* image no longer
//! changes how big the shape reads relative to its surroundings — see
//! `docs/wave-visibility-evidence/README.md` for the evidence set this
//! produced and why the trap mattered there.
//!
//! ```bash
//! cargo run --example desktop_composite -- --bg dark --canvas 700x300 \
//!     out/ frame.png
//! ```

use std::env;
use std::path::Path;

use tiny_skia::{Pixmap, PremultipliedColorU8};

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: desktop_composite [--bg light|dark|mid] [--canvas WxH] <out_dir> <in.png>..."
        );
        std::process::exit(1);
    }
    let bg = if args[0] == "--bg" {
        let v = args[1].clone();
        args.drain(0..2);
        v
    } else {
        "mid".to_string()
    };
    let (br, bgc, bb) = match bg.as_str() {
        "light" => (240u8, 241u8, 245u8),
        "dark" => (18u8, 19u8, 22u8),
        _ => (60u8, 62u8, 68u8),
    };
    let (cw, ch) = if args[0] == "--canvas" {
        let (w, h) = args[1].split_once('x').expect("--canvas WxH");
        let dims = (w.parse().unwrap(), h.parse().unwrap());
        args.drain(0..2);
        dims
    } else {
        (1920u32, 1080u32)
    };
    let out_dir = Path::new(&args[0]);
    std::fs::create_dir_all(out_dir).unwrap();

    for in_path in &args[1..] {
        let src = Pixmap::load_png(in_path).unwrap_or_else(|e| panic!("{in_path}: {e}"));

        let mut canvas = Pixmap::new(cw, ch).unwrap();
        for p in canvas.pixels_mut() {
            *p = PremultipliedColorU8::from_rgba(br, bgc, bb, 255).unwrap();
        }

        // The frame is premultiplied, so source-over is `src + dst * (1 -
        // a)`, and the canvas is opaque, so the result stays opaque —
        // matching `pill_demo.rs`'s `write_frame`.
        let ox = (cw - src.width()) / 2;
        let oy = (ch - src.height()) / 2;
        let dst = canvas.pixels_mut();
        for y in 0..src.height() {
            for x in 0..src.width() {
                let s = src.pixels()[(y * src.width() + x) as usize];
                let (dx, dy) = (ox + x, oy + y);
                let inv = 1.0 - f32::from(s.alpha()) / 255.0;
                let d = dst[(dy as usize) * cw as usize + dx as usize];
                let blend =
                    |sc: u8, dc: u8| (f32::from(sc) + f32::from(dc) * inv).round().min(255.0) as u8;
                dst[(dy as usize) * cw as usize + dx as usize] = PremultipliedColorU8::from_rgba(
                    blend(s.red(), d.red()),
                    blend(s.green(), d.green()),
                    blend(s.blue(), d.blue()),
                    255,
                )
                .unwrap();
            }
        }

        let stem = Path::new(in_path).file_stem().unwrap().to_string_lossy();
        let out_path = out_dir.join(format!("{stem}-on-desktop.png"));
        canvas.save_png(&out_path).unwrap();
        println!("{} -> {}", in_path, out_path.display());
    }
}
