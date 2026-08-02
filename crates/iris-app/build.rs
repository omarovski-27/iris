//! Embeds the prism app icon and version metadata into the Windows `.exe`.
//!
//! A build script cannot depend on the crate it builds, so the icon geometry
//! below is a deliberate, minimal duplicate of `tray::icon_rgba` (dark plate
//! only — the file icon does not switch with the app theme). Keep the two in
//! sync by hand if the mark ever changes; nothing enforces that automatically.
//!
//! Uses `winresource`, which drives the mingw `windres` already required to
//! link `x86_64-pc-windows-gnu` (see `docs/dev-windows.md`) — no MSVC, no
//! Windows SDK, no new host dependency.

use std::env;
use std::io::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let ico_path = Path::new(&out_dir).join("iris.ico");
    write_ico(&ico_path, &[16, 32, 48, 256]);

    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION set by cargo");
    let (major, minor, patch) = parse_version(&version);
    let packed = (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("OUT_DIR is valid UTF-8"))
        .set("ProductName", "Iris")
        .set("FileDescription", "Iris - push-to-talk dictation")
        .set("OriginalFilename", "iris.exe")
        .set("InternalName", "iris")
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    // Fatal, not a cargo:warning. The icon and the version metadata are the
    // whole reason this build script exists, and a warning in a release build's
    // output is easy to miss — the failure would only surface as a shipped exe
    // with a generic icon and blank Properties, after the zip was cut.
    if let Err(e) = res.compile() {
        let tool = match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => "the Windows SDK's `rc.exe`",
            _ => "the mingw `windres` that linking x86_64-pc-windows-gnu already requires",
        };
        panic!(
            "could not embed the Windows resources (icon and version metadata): {e}\n\
             winresource drives {tool} — see docs/dev-windows.md."
        );
    }
}

fn parse_version(v: &str) -> (u16, u16, u16) {
    let mut parts = v.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Writes a multi-size `.ico` built from the prism-triangle geometry, dark
/// plate. See `bmp_dib` for the per-image encoding.
fn write_ico(path: &Path, sizes: &[u32]) {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&s| (s, bmp_dib(s, &icon_rgba(s))))
        .collect();

    let mut file = std::fs::File::create(path).expect("creating the generated icon.ico");
    file.write_all(&0u16.to_le_bytes()).unwrap(); // ICONDIR.reserved
    file.write_all(&1u16.to_le_bytes()).unwrap(); // ICONDIR.type = icon
    file.write_all(&(images.len() as u16).to_le_bytes())
        .unwrap();

    let header_size = 6 + 16 * images.len();
    let mut offset = header_size as u32;
    let mut entries = Vec::new();
    for (size, bmp) in &images {
        entries.push((*size, bmp.len() as u32, offset));
        offset += bmp.len() as u32;
    }
    for (size, len, off) in &entries {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        file.write_all(&[dim, dim, 0, 0]).unwrap(); // width, height, colours, reserved
        file.write_all(&1u16.to_le_bytes()).unwrap(); // planes
        file.write_all(&32u16.to_le_bytes()).unwrap(); // bit count
        file.write_all(&len.to_le_bytes()).unwrap();
        file.write_all(&off.to_le_bytes()).unwrap();
    }
    for (_, bmp) in &images {
        file.write_all(bmp).unwrap();
    }
}

/// One `ICONIMAGE`: a `BITMAPINFOHEADER` followed by bottom-up BGRA pixels and
/// a 1bpp AND mask. The mask is all-zero (opaque) — real transparency comes
/// from the alpha channel, which every Vista-or-later reader honours; the mask
/// is only present because the file format still requires it.
fn bmp_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight: XOR + AND
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(size * size * 4).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // biX/YPelsPerMeter, biClrUsed, biClrImportant

    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let (r, g, b, a) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    let row_bytes = size.div_ceil(32) * 4;
    out.extend(std::iter::repeat_n(0u8, (row_bytes * size) as usize));
    out
}

// --- everything below mirrors crates/iris-app/src/tray.rs::icon_rgba,
// point_in_triangle and spectrum_sample (dark theme only) ---

fn icon_rgba(size: u32) -> Vec<u8> {
    let (plate_r, plate_g, plate_b, plate_a) = (20u8, 24u8, 34u8, 255u8); // #141822

    let s = size as f32;
    let pad = s * 0.12;
    let apex = (s * 0.5, pad + s * 0.08);
    let bl = (pad + s * 0.08, s - pad);
    let br = (s - pad - s * 0.08, s - pad);

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let inside = point_in_triangle(px, py, apex, bl, br);
            if inside {
                let t = ((px - bl.0) / (br.0 - bl.0)).clamp(0.0, 1.0);
                let (r, g, b) = spectrum_sample(t);
                let sheen = 1.0 + 0.12 * (1.0 - ((py - apex.1) / (bl.1 - apex.1)).clamp(0.0, 1.0));
                let r = ((r as f32) * sheen).min(255.0) as u8;
                let g = ((g as f32) * sheen).min(255.0) as u8;
                let b = ((b as f32) * sheen).min(255.0) as u8;
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else {
                let cx = s * 0.5;
                let cy = s * 0.5;
                let half = s * 0.48;
                let dx = (px - cx).abs();
                let dy = (py - cy).abs();
                let edge = (half - dx.max(dy)).clamp(0.0, 1.0);
                let alpha = (edge * f32::from(plate_a)) as u8;
                rgba.extend_from_slice(&[plate_r, plate_g, plate_b, alpha]);
            }
        }
    }
    rgba
}

fn point_in_triangle(px: f32, py: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let v0 = (c.0 - a.0, c.1 - a.1);
    let v1 = (b.0 - a.0, b.1 - a.1);
    let v2 = (px - a.0, py - a.1);
    let dot00 = v0.0 * v0.0 + v0.1 * v0.1;
    let dot01 = v0.0 * v1.0 + v0.1 * v1.1;
    let dot02 = v0.0 * v2.0 + v0.1 * v2.1;
    let dot11 = v1.0 * v1.0 + v1.1 * v1.1;
    let dot12 = v1.0 * v2.0 + v1.1 * v2.1;
    let denom = dot00 * dot11 - dot01 * dot01;
    if denom.abs() < f32::EPSILON {
        return false;
    }
    let inv = 1.0 / denom;
    let u = (dot11 * dot02 - dot01 * dot12) * inv;
    let v = (dot00 * dot12 - dot01 * dot02) * inv;
    u >= 0.0 && v >= 0.0 && (u + v) <= 1.0
}

fn spectrum_sample(t: f32) -> (u8, u8, u8) {
    const STOPS: [(f32, u8, u8, u8); 5] = [
        (0.00, 255, 107, 138),
        (0.25, 255, 184, 107),
        (0.50, 107, 255, 184),
        (0.75, 107, 203, 255),
        (1.00, 139, 123, 255),
    ];
    let t = t.clamp(0.0, 1.0);
    for i in 0..STOPS.len() - 1 {
        let (t0, r0, g0, b0) = STOPS[i];
        let (t1, r1, g1, b1) = STOPS[i + 1];
        if t <= t1 {
            let u = if (t1 - t0).abs() < f32::EPSILON {
                0.0
            } else {
                (t - t0) / (t1 - t0)
            };
            let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * u) as u8;
            return (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1));
        }
    }
    let last = STOPS[STOPS.len() - 1];
    (last.1, last.2, last.3)
}
