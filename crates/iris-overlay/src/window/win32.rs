//! The Windows overlay window.
//!
//! A layered, per-pixel-alpha, click-through, never-activating tool window,
//! updated with `UpdateLayeredWindow` from a CPU-rasterised premultiplied
//! bitmap. The checklist from the design report, and where each item lands:
//!
//! | Requirement | How |
//! |---|---|
//! | Per-pixel alpha | `WS_EX_LAYERED` + `UpdateLayeredWindow` with `ULW_ALPHA` |
//! | Click-through | `WS_EX_TRANSPARENT`, plus `HTTRANSPARENT` from `WM_NCHITTEST` |
//! | Never activates | `WS_EX_NOACTIVATE` and `SW_SHOWNOACTIVATE`; no `SetForegroundWindow` |
//! | Out of Alt-Tab | `WS_EX_TOOLWINDOW` |
//! | Always on top | `WS_EX_TOPMOST` |
//! | DPI | per-monitor-V2 on this thread; geometry rebuilt from the monitor's real dpi |
//!
//! There is no `WM_PAINT` handler and the window has no background brush:
//! a layered window's content lives entirely in the bitmap handed to
//! `UpdateLayeredWindow`, so painting is driven by our frame loop rather than
//! by invalidation.
//!
//! This module never synthesises input. See the repository's `CLAUDE.md`.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tiny_skia::Pixmap;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetMonitorInfoW,
    MonitorFromWindow, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow,
    HTTRANSPARENT, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NCHITTEST, WM_SETTINGCHANGE,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

use crate::handle::OverlayConfig;
use crate::layout::{Placement, WorkArea};
use crate::motion::{FRAME_INTERVAL_MS, IDLE_POLL_MS};
use crate::render::Renderer;
use crate::state::{Command, Model};
use crate::OverlayError;

/// How many consecutive failed presents before the overlay gives up.
///
/// A single failure is a transient GDI hiccup (a session switch, a monitor
/// being unplugged mid-frame). A sustained run of them means the surface is
/// unusable and spinning on it would just burn a core.
const MAX_PRESENT_FAILURES: u32 = 120;

thread_local! {
    /// Set by the window procedure when the monitor layout or dpi may have
    /// changed; cleared by the loop once it has re-measured.
    static PLACEMENT_DIRTY: Cell<bool> = const { Cell::new(true) };
}

pub(crate) fn run(
    rx: Receiver<Command>,
    config: OverlayConfig,
    ready: &Sender<Result<(), OverlayError>>,
) {
    // Per-monitor-V2 for this thread only. The overlay is a library: the host
    // process may have no manifest, or a different awareness, and we must not
    // change process-wide state on its behalf.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mut surface = match Surface::create() {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let mut model = Model::new(config.theme);
    if !config.engine.is_empty() {
        model.apply(Command::Engine(config.engine));
    }
    let mut renderer = Renderer::new(1.0);
    let mut placement = Placement::compute(primary_fallback(), 1.0);
    let mut visible = false;
    let mut failures = 0u32;

    let start = Instant::now();
    loop {
        pump_messages();

        if !super::drain(&rx, &mut model) {
            break;
        }

        model.tick(elapsed_ms(start));

        if model.is_idle() {
            if visible {
                surface.hide();
                visible = false;
            }
            // Nothing to draw: park on the channel. The short timeout keeps the
            // message pump ticking over so a dpi or display change is not
            // noticed only when the user next speaks.
            let until = Instant::now() + Duration::from_millis(IDLE_POLL_MS);
            if !super::drain_until(&rx, &mut model, until) {
                break;
            }
            continue;
        }

        // Re-measure on the way in, and whenever Windows tells us the desktop
        // moved underneath us.
        if !visible || PLACEMENT_DIRTY.with(Cell::take) {
            let (work, dpi) = surface.target_monitor();
            renderer.set_scale(dpi as f32 / 96.0);
            placement = Placement::compute(work, renderer.layout().scale);
        }

        let frame_start = Instant::now();
        let frame = renderer.draw(&model);
        match surface.present(frame, placement) {
            Ok(()) => failures = 0,
            Err(_) => {
                failures += 1;
                if failures >= MAX_PRESENT_FAILURES {
                    break;
                }
            }
        }

        if !visible {
            surface.show();
            visible = true;
        }

        // Pace to ~60 fps from the top of this frame, so a frame that took
        // 6 ms is followed by a 10 ms wait rather than a fresh 16 ms one.
        // Commands that arrive meanwhile are applied and shown next frame.
        let next_frame = frame_start + Duration::from_millis(FRAME_INTERVAL_MS);
        if !super::drain_until(&rx, &mut model, next_frame) {
            break;
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Drain the thread's message queue without blocking.
fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message pump on the thread that owns the window.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The work area used before the window exists, and if a monitor query fails.
fn primary_fallback() -> WorkArea {
    WorkArea {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    }
}

// ---------------------------------------------------------------------------
// the window and its bitmap
// ---------------------------------------------------------------------------

/// The layered window plus the DIB section it is updated from.
struct Surface {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    size: (i32, i32),
}

impl Surface {
    fn create() -> Result<Self, OverlayError> {
        // SAFETY: every call below is checked; handles are stored and released
        // in `Drop`.
        unsafe {
            let instance = GetModuleHandleW(None)
                .map_err(|e| OverlayError::Platform(format!("GetModuleHandleW failed: {e}")))?;

            let class = w!("IrisOverlayPill");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // A zero return is only fatal if the class is not already ours from
            // an earlier `spawn` in this process.
            let _ = RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                class,
                w!("Iris"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| OverlayError::Platform(format!("CreateWindowExW failed: {e}")))?;

            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                let _ = DestroyWindow(hwnd);
                return Err(OverlayError::Platform(
                    "CreateCompatibleDC failed".to_string(),
                ));
            }

            Ok(Self {
                hwnd,
                dc,
                bitmap: HBITMAP::default(),
                previous: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                size: (0, 0),
            })
        }
    }

    /// The work area and dpi of the monitor the user is actually working on.
    ///
    /// Follows the foreground window, so on a multi-monitor desk the pill
    /// appears under the app being dictated into rather than always on the
    /// primary.
    fn target_monitor(&self) -> (WorkArea, u32) {
        // SAFETY: read-only monitor queries; `MONITORINFO` is initialised with
        // its required `cbSize` before use.
        unsafe {
            let foreground = GetForegroundWindow();
            let anchor = if foreground.0.is_null() {
                self.hwnd
            } else {
                foreground
            };
            let monitor = MonitorFromWindow(anchor, MONITOR_DEFAULTTOPRIMARY);

            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let work = if GetMonitorInfoW(monitor, &mut info).as_bool() {
                rect_to_work_area(info.rcWork)
            } else {
                primary_fallback()
            };

            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            (work, if dpi_x == 0 { 96 } else { dpi_x })
        }
    }

    /// (Re)allocate the DIB section when the window size changes.
    fn ensure_bitmap(&mut self, width: i32, height: i32) -> Result<(), OverlayError> {
        if self.size == (width, height) && !self.bits.is_null() {
            return Ok(());
        }
        if width <= 0 || height <= 0 {
            return Err(OverlayError::Platform("zero-sized overlay".to_string()));
        }

        // SAFETY: the previous bitmap, if any, is deselected before deletion,
        // and the new one is selected into our own memory DC.
        unsafe {
            self.release_bitmap();

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // Negative height: a top-down DIB, so its row order matches
                    // the rasteriser's and the copy is a straight memcpy.
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|e| OverlayError::Platform(format!("CreateDIBSection failed: {e}")))?;
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                return Err(OverlayError::Platform(
                    "CreateDIBSection returned no pixels".to_string(),
                ));
            }

            self.previous = SelectObject(self.dc, bitmap.into());
            self.bitmap = bitmap;
            self.bits = bits.cast::<u8>();
            self.size = (width, height);
        }
        Ok(())
    }

    /// SAFETY: caller must not use `self.bits` afterwards without reallocating.
    unsafe fn release_bitmap(&mut self) {
        if !self.bitmap.is_invalid() {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            let _ = DeleteObject(self.bitmap.into());
        }
        self.bitmap = HBITMAP::default();
        self.previous = HGDIOBJ::default();
        self.bits = std::ptr::null_mut();
        self.size = (0, 0);
    }

    /// Blit one rendered frame to the screen, moving and resizing the window to
    /// `placement` in the same call.
    fn present(&mut self, frame: &Pixmap, placement: Placement) -> Result<(), OverlayError> {
        let width = i32::try_from(placement.width).unwrap_or(i32::MAX);
        let height = i32::try_from(placement.height).unwrap_or(i32::MAX);
        if frame.width() != placement.width || frame.height() != placement.height {
            return Err(OverlayError::Platform(
                "frame size does not match placement".to_string(),
            ));
        }
        self.ensure_bitmap(width, height)?;

        // SAFETY: `self.bits` points at `width * height * 4` bytes owned by the
        // DIB section we just ensured, and nothing else aliases it.
        unsafe {
            let len = (width as usize) * (height as usize) * 4;
            let dst = std::slice::from_raw_parts_mut(self.bits, len);
            // tiny-skia is premultiplied RGBA; a DIB is premultiplied BGRA.
            for (out, src) in dst.chunks_exact_mut(4).zip(frame.data().chunks_exact(4)) {
                out[0] = src[2];
                out[1] = src[1];
                out[2] = src[0];
                out[3] = src[3];
            }

            let destination = POINT {
                x: placement.x,
                y: placement.y,
            };
            let size = SIZE {
                cx: width,
                cy: height,
            };
            let source = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&destination),
                Some(&size),
                Some(self.dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .map_err(|e| OverlayError::Platform(format!("UpdateLayeredWindow failed: {e}")))
        }
    }

    fn show(&self) {
        // SAFETY: plain window-state calls on our own HWND. `SW_SHOWNOACTIVATE`
        // and `SWP_NOACTIVATE` are what keep the caret in the target app.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    fn hide(&self) {
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: every handle here was created by this type and is released
        // exactly once.
        unsafe {
            self.release_bitmap();
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
            if !self.hwnd.0.is_null() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn rect_to_work_area(rect: RECT) -> WorkArea {
    WorkArea {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// The window procedure.
///
/// Deliberately tiny: it answers hit tests as transparent and records that the
/// desktop geometry may have moved. Everything else is the frame loop's job.
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Belt and braces alongside WS_EX_TRANSPARENT: the pill is a display,
        // never a target for the mouse.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            PLACEMENT_DIRTY.with(|dirty| dirty.set(true));
            LRESULT(0)
        }
        // SAFETY: forwarding an unhandled message with its original arguments.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
