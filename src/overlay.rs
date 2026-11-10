// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Drawing on the screen while recording.
//!
//! [`DrawOverlay`] creates a transparent, always-on-top layered window over the recorded area.
//! What is drawn on it is part of the screen, so screen capture records it. While drawing is
//! off the window is click-through (clicks reach the programs below); while it is on, the
//! window takes the mouse and the keyboard, except over the control bar. The window runs on its
//! own thread with a plain Win32 message loop and is painted with tiny-skia; the app changes
//! tool, colour and mode through shared state that the thread polls on a timer. Window capture
//! does not see this window, so the recorder blends the same drawings into its frames with
//! [`blend_layer`].

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tiny_skia::{Color, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_BACK, VK_DELETE, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GetMessageW,
    GetWindowLongPtrW, IDC_CROSS, KillTimer, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SW_SHOWNOACTIVATE, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect, WM_DESTROY,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::w;

/// True while drawing mode is on; the recorder then neither shows click ripples nor logs
/// clicks, because those clicks are pen strokes.
pub static DRAWING: AtomicBool = AtomicBool::new(false);

/// The current drawings as a [`Layer`], or `None` when nothing is drawn.
///
/// Updated by the window thread on every repaint and read by the recorder for window capture,
/// which does not see the drawing window.
static LAYER: Mutex<Option<Arc<Layer>>> = Mutex::new(None);
/// Set by the first [`blend_layer`] call. Only window recordings use [`LAYER`], so the window
/// thread copies its drawings there only once one has asked for them.
static LAYER_WANTED: AtomicBool = AtomicBool::new(false);
/// A rendered drawing layer: (width, height, premultiplied BGRA pixels).
type Layer = (u32, u32, Vec<u8>);

/// Blends the current drawings onto a BGRA frame of `w` x `h` pixels.
///
/// Called by the recorder for each frame of a window recording. Does nothing if there are no
/// drawings or if the layer size differs from the frame (for example while the overlay is
/// being resized); the frame's alpha channel is left unchanged.
pub fn blend_layer(frame: &mut [u8], w: u32, h: u32) {
    LAYER_WANTED.store(true, Ordering::Relaxed);
    // Clone the Arc and release the lock at once so that the window thread is not blocked.
    let layer = LAYER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(layer) = layer else { return };
    let (lw, lh, px) = &*layer;
    if (*lw, *lh) != (w, h) || px.len() != frame.len() {
        return;
    }
    // Premultiplied source-over: dst = src + dst * (1 - src_alpha).
    for (d, s) in frame.chunks_exact_mut(4).zip(px.chunks_exact(4)) {
        let a = s[3] as u32;
        // Alpha 1 is the invisible tint that makes the window take the mouse, not a drawing.
        if a <= 1 {
            continue;
        }
        for c in 0..3 {
            d[c] = (s[c] as u32 + d[c] as u32 * (255 - a) / 255).min(255) as u8;
        }
    }
}

/// A drawing tool for the on-screen overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawTool {
    /// Opaque freehand line.
    Pen,
    /// Wide, translucent freehand line.
    Highlighter,
    /// Straight arrow from the press point to the release point.
    Arrow,
    /// Rectangle outline spanned by the press and release points.
    Rectangle,
    /// Ellipse outline inscribed in the rectangle spanned by the drag.
    Ellipse,
}

impl DrawTool {
    /// Every tool in toolbar order.
    pub const ALL: [DrawTool; 5] = [
        DrawTool::Pen,
        DrawTool::Highlighter,
        DrawTool::Arrow,
        DrawTool::Rectangle,
        DrawTool::Ellipse,
    ];

    /// Returns the symbol shown on the tool's button.
    pub fn icon(self) -> &'static str {
        match self {
            DrawTool::Pen => "✏",
            DrawTool::Highlighter => "🖊",
            DrawTool::Arrow => "↗",
            DrawTool::Rectangle => "🔲",
            DrawTool::Ellipse => "⭕",
        }
    }

    /// Returns the translated tool name for tooltips.
    pub fn label(self) -> &'static str {
        use crate::i18n::tr;
        match self {
            DrawTool::Pen => tr("Pen"),
            DrawTool::Highlighter => tr("Highlighter"),
            DrawTool::Arrow => tr("Arrow"),
            DrawTool::Rectangle => tr("Rectangle"),
            DrawTool::Ellipse => tr("Ellipse"),
        }
    }

    /// Returns the virtual-key code that selects the tool while drawing (P, H, A, R, E).
    fn key(self) -> u16 {
        match self {
            DrawTool::Pen => b'P' as u16,
            DrawTool::Highlighter => b'H' as u16,
            DrawTool::Arrow => b'A' as u16,
            DrawTool::Rectangle => b'R' as u16,
            DrawTool::Ellipse => b'E' as u16,
        }
    }
}

/// Pen colours as RGB, selected with keys 1 to 6 while drawing.
pub const COLORS: [[u8; 3]; 6] = [
    [239, 35, 60],   // red
    [255, 196, 0],   // yellow
    [46, 204, 64],   // green
    [33, 150, 243],  // blue
    [255, 255, 255], // white
    [20, 20, 20],    // black
];

/// One drawn shape in window-local physical pixels.
#[derive(Debug, Clone)]
struct Shape {
    /// Tool the shape was drawn with; decides how `points` are interpreted.
    tool: DrawTool,
    /// Stroke colour as RGB (one of [`COLORS`]).
    color: [u8; 3],
    /// For freehand tools, the stroke's points; for the others, the press point followed by
    /// the current or release point. Never empty.
    points: Vec<(f32, f32)>,
}

/// State shared between the app (via [`DrawOverlay`]) and the window thread.
///
/// The window thread reads it on each timer tick; `dirty` says that a repaint is needed.
struct Shared {
    /// Whether drawing mode is on (the window takes mouse and keyboard input).
    drawing: bool,
    /// Tool used for the next shape.
    tool: DrawTool,
    /// Index into [`COLORS`].
    color: usize,
    /// Finished shapes, oldest first.
    shapes: Vec<Shape>,
    /// Incremented whenever `shapes` changes, so the window thread knows when to re-render
    /// its cached image of the finished shapes.
    version: u64,
    /// Area (window pixels) left click-through while drawing: the control bar.
    hole: Option<(i32, i32, i32, i32)>,
    /// New desktop position (the recorded window moved).
    pos: Option<(i32, i32)>,
    /// Something changed and the window must be repainted.
    dirty: bool,
    /// The window should close and the thread end.
    quit: bool,
}

/// Handle to the drawing window. Dropping it closes the window.
pub struct DrawOverlay {
    /// State shared with the window thread.
    shared: Arc<Mutex<Shared>>,
    /// The window thread, joined on drop; `None` if it could not be spawned.
    thread: Option<JoinHandle<()>>,
    /// Current desktop position of the window's top-left corner, for converting screen
    /// rectangles into window coordinates and skipping redundant moves.
    origin: std::cell::Cell<(i32, i32)>,
}

impl DrawOverlay {
    /// Creates the drawing window over the screen rectangle `x, y, w, h` (physical pixels).
    ///
    /// The window starts click-through with no drawings, the pen tool and the first colour.
    /// It is created on a new thread; failures are logged and leave the overlay inert.
    pub fn start(x: i32, y: i32, w: i32, h: i32) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            drawing: false,
            tool: DrawTool::Pen,
            color: 0,
            shapes: vec![],
            version: 0,
            hole: None,
            pos: None,
            dirty: true,
            quit: false,
        }));
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("draw-overlay".into())
            .spawn(move || {
                if let Err(e) = run(s, x, y, w.max(1), h.max(1)) {
                    log::error!("drawing overlay: {e:#}");
                }
            })
            .ok();
        Self {
            shared,
            thread,
            origin: std::cell::Cell::new((x, y)),
        }
    }

    /// Changes the shared state with `f` and marks the window for repainting.
    fn update(&self, f: impl FnOnce(&mut Shared)) {
        if let Ok(mut s) = self.shared.lock() {
            f(&mut s);
            s.dirty = true;
        }
    }

    /// Reads a value from the shared state; `None` if the lock is poisoned.
    fn read<T>(&self, f: impl FnOnce(&Shared) -> T) -> Option<T> {
        self.shared.lock().ok().map(|s| f(&s))
    }

    /// Returns whether drawing mode is on. The user can also turn it off with Esc or a
    /// right-click in the window.
    pub fn is_drawing(&self) -> bool {
        self.read(|s| s.drawing).unwrap_or(false)
    }

    /// Turns drawing mode on (the window takes mouse and keyboard) or off (click-through).
    /// Existing drawings stay visible either way.
    pub fn set_drawing(&self, on: bool) {
        self.update(|s| s.drawing = on);
    }

    /// Returns the selected tool.
    pub fn tool(&self) -> DrawTool {
        self.read(|s| s.tool).unwrap_or(DrawTool::Pen)
    }

    /// Selects the tool for the next shape.
    pub fn set_tool(&self, tool: DrawTool) {
        self.update(|s| s.tool = tool);
    }

    /// Returns the selected colour as an index into [`COLORS`].
    pub fn color(&self) -> usize {
        self.read(|s| s.color).unwrap_or(0)
    }

    /// Selects colour `i` of [`COLORS`] for the next shape; out-of-range values select the last.
    pub fn set_color(&self, i: usize) {
        self.update(|s| s.color = i.min(COLORS.len() - 1));
    }

    /// Returns whether anything has been drawn.
    pub fn has_shapes(&self) -> bool {
        self.read(|s| !s.shapes.is_empty()).unwrap_or(false)
    }

    /// Removes the most recent shape.
    pub fn undo(&self) {
        self.update(|s| {
            s.shapes.pop();
            s.version += 1;
        });
    }

    /// Removes all shapes.
    pub fn clear(&self) {
        self.update(|s| {
            s.shapes.clear();
            s.version += 1;
        });
    }

    /// Moves the window's top-left corner to (`x`, `y`) in desktop pixels, so that it follows
    /// a recorded window that moved. The size is unchanged.
    pub fn move_to(&self, x: i32, y: i32) {
        if self.origin.get() != (x, y) {
            self.origin.set((x, y));
            self.update(|s| s.pos = Some((x, y)));
        }
    }

    /// Keeps the screen rectangle `(x, y, w, h)` (physical pixels) clickable while drawing,
    /// so that the control bar can still be used; `None` removes it.
    pub fn set_hole(&self, rect: Option<(i32, i32, i32, i32)>) {
        // Convert to window coordinates and only repaint when the hole actually changed,
        // since the app calls this every frame.
        let (ox, oy) = self.origin.get();
        let local = rect.map(|(x, y, w, h)| (x - ox, y - oy, w, h));
        if self.read(|s| s.hole) != Some(local) {
            self.update(|s| s.hole = local);
        }
    }
}

/// Asks the window thread to close, waits for it, and resets the global drawing state so
/// that the recorder stops blending drawings and ignoring clicks.
impl Drop for DrawOverlay {
    fn drop(&mut self) {
        self.update(|s| s.quit = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        DRAWING.store(false, Ordering::Relaxed);
        LAYER_WANTED.store(false, Ordering::Relaxed);
        *LAYER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// ----------------------------------------------------------------------
// The window thread

/// State owned by the window thread: the window, its position and size, the shape in
/// progress and the GDI bitmap the window is painted from.
struct Local {
    /// The drawing window.
    hwnd: HWND,
    /// Desktop x coordinate of the window's top-left corner, in physical pixels.
    x: i32,
    /// Desktop y coordinate of the window's top-left corner, in physical pixels.
    y: i32,
    /// Window width in physical pixels.
    w: i32,
    /// Window height in physical pixels.
    h: i32,
    /// State shared with the app through [`DrawOverlay`].
    shared: Arc<Mutex<Shared>>,
    /// Shape being drawn with the mouse.
    current: Option<Shape>,
    /// Drawing mode the window style currently reflects.
    applied: bool,
    /// DPI scale of the window's monitor (1.0 at 96 DPI); line widths are multiplied by it.
    scale: f32,
    /// Memory device context holding `dib`, the source for `UpdateLayeredWindow`.
    mem: HDC,
    /// 32-bit top-down DIB section of the window size.
    dib: HBITMAP,
    /// The bitmap `mem` held before `dib` was selected into it; restored before deleting `dib`.
    old: HGDIOBJ,
    /// Pixel memory of `dib` (w * h * 4 bytes, premultiplied BGRA).
    bits: *mut u8,
    /// The finished shapes rendered once (see [`Base`]); rebuilt only when they change.
    base: Option<Base>,
    /// What the window shows: `base` plus the shape in progress. While a shape is drawn only
    /// the area it covers is restored from `base`, redrawn and sent to the window.
    canvas: Option<Pixmap>,
    /// Area (window pixels) the shape in progress covered at the last repaint.
    prev_rect: Option<PxRect>,
    /// When the window was last repainted, to repaint straight from mouse moves at most about
    /// 120 times per second.
    last_paint: Instant,
    /// The canvas changed since it was last copied to [`LAYER`].
    layer_stale: bool,
    /// When the canvas was last copied to [`LAYER`] (at most 25 times per second).
    last_layer: Instant,
}

/// The finished shapes rendered into window-sized premultiplied BGRA pixels (colours are given
/// to tiny-skia with red and blue swapped, so no conversion is needed). While drawing mode is
/// on, empty pixels carry an alpha-1 tint so the window takes the mouse, except over the
/// control bar.
struct Base {
    /// Shape list version, drawing mode and control bar area the pixels were made for.
    key: BaseKey,
    /// The rendered pixels, window-sized and premultiplied BGRA.
    pixels: Pixmap,
    /// Whether there are any finished shapes.
    has_shapes: bool,
}

/// See [`Base::key`].
type BaseKey = (u64, bool, Option<(i32, i32, i32, i32)>);

/// A pixel rectangle `(x0, y0, x1, y1)`, end-exclusive, inside the window.
type PxRect = (i32, i32, i32, i32);

thread_local! {
    /// The window thread's state, reachable from the window procedure.
    static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
}

/// Runs `f` on the window thread's state.
///
/// Returns `None` if there is no state yet or it is already borrowed (a message delivered
/// re-entrantly while a handler runs); such messages are skipped.
fn with_local<R>(f: impl FnOnce(&mut Local) -> R) -> Option<R> {
    LOCAL.with(|l| l.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Body of the window thread: creates the layered window and its bitmap, runs the message
/// loop until the window is destroyed, then frees the GDI objects.
fn run(shared: Arc<Mutex<Shared>>, x: i32, y: i32, w: i32, h: i32) -> anyhow::Result<()> {
    // SAFETY: Win32 window and GDI calls on this thread with valid arguments; the DIB's pixel
    // pointer stays valid until the DIB is deleted after the message loop.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("SoftromedaDrawOverlay");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_CROSS)?,
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly if already registered
        // Layered (per-pixel alpha), topmost, kept out of the taskbar and Alt+Tab, and
        // initially click-through and non-activating.
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            class,
            w!("Drawing"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;

        // A top-down (negative height) 32-bit bitmap the window is painted from.
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(mem, dib.into());

        let dpi = GetDpiForWindow(hwnd).max(96);
        LOCAL.with(|l| {
            *l.borrow_mut() = Some(Local {
                hwnd,
                x,
                y,
                w,
                h,
                shared,
                current: None,
                applied: false,
                scale: dpi as f32 / 96.0,
                mem,
                dib,
                old,
                bits: bits as *mut u8,
                base: None,
                canvas: None,
                prev_rect: None,
                last_paint: Instant::now(),
                layer_stale: true,
                last_layer: Instant::now(),
            })
        });
        // Show without taking focus from the recorded program, and poll the shared state
        // about 60 times per second.
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 15, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Window destroyed: release the bitmap and memory DC.
        if let Some(l) = LOCAL.with(|l| l.borrow_mut().take()) {
            SelectObject(l.mem, l.old);
            let _ = DeleteObject(l.dib.into());
            let _ = DeleteDC(l.mem);
        }
    }
    Ok(())
}

/// Extracts the signed client coordinates packed into a mouse message's `LPARAM`.
fn lparam_point(lp: LPARAM) -> (f32, f32) {
    let x = (lp.0 & 0xffff) as u16 as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as u16 as i16 as f32;
    (x, y)
}

/// Window procedure of the drawing window.
///
/// Mouse messages only arrive while drawing mode is on (otherwise the window is
/// click-through). The left button draws shapes, the right button ends drawing mode, and keys
/// select tools and colours, undo and clear.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            with_local(tick);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Capture the mouse so that the drag continues if the pointer leaves the window,
            // and start a new shape with the current tool and colour at the press point.
            // SAFETY: `hwnd` is this window, and the call is made on its own thread.
            unsafe { SetCapture(hwnd) };
            let p = lparam_point(lp);
            with_local(|l| {
                let (tool, color) = l
                    .shared
                    .lock()
                    .map(|s| (s.tool, COLORS[s.color]))
                    .unwrap_or((DrawTool::Pen, COLORS[0]));
                l.current = Some(Shape {
                    tool,
                    color,
                    points: vec![p, p],
                });
                mark_dirty(l);
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let p = lparam_point(lp);
            with_local(|l| {
                if let Some(c) = &mut l.current {
                    match c.tool {
                        // Freehand: append points, skipping moves under 1.5 pixels to keep
                        // the path short.
                        DrawTool::Pen | DrawTool::Highlighter => {
                            let last = *c.points.last().unwrap_or(&p);
                            if (last.0 - p.0).abs() + (last.1 - p.1).abs() >= 1.5 {
                                c.points.push(p);
                            }
                        }
                        // Two-point shapes: move the end point.
                        _ => {
                            if let Some(end) = c.points.last_mut() {
                                *end = p;
                            }
                        }
                    }
                    // Repaint immediately rather than on the next timer tick: WM_TIMER messages
                    // are delayed while mouse input keeps arriving. The 8 ms floor caps these
                    // direct repaints at about 120 per second.
                    if l.last_paint.elapsed() >= Duration::from_millis(8) {
                        paint(l);
                    } else {
                        mark_dirty(l);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // Finish the shape and add it to the shared list.
            // SAFETY: releasing the capture taken in WM_LBUTTONDOWN; no arguments.
            let _ = unsafe { ReleaseCapture() };
            with_local(|l| {
                if let Some(c) = l.current.take() {
                    if let Ok(mut s) = l.shared.lock() {
                        s.shapes.push(c);
                        s.version += 1;
                        s.dirty = true;
                    }
                }
            });
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            // Right-click: stop drawing (the drawings stay on screen).
            with_local(|l| {
                if let Ok(mut s) = l.shared.lock() {
                    s.drawing = false;
                    s.dirty = true;
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // Keyboard shortcuts: Esc ends drawing, Ctrl+Z undoes, Delete/Backspace/C clear,
            // 1 to 6 pick a colour and P/H/A/R/E pick a tool. 0x11 is VK_CONTROL; a negative
            // key state means the key is down.
            let key = wp.0 as u16;
            // SAFETY: GetKeyState only reads this thread's keyboard state.
            let ctrl =
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) < 0 };
            with_local(|l| {
                if let Ok(mut s) = l.shared.lock() {
                    if key == VK_ESCAPE.0 {
                        s.drawing = false;
                    } else if ctrl && key == b'Z' as u16 {
                        s.shapes.pop();
                        s.version += 1;
                    } else if key == VK_DELETE.0 || key == VK_BACK.0 || key == b'C' as u16 {
                        s.shapes.clear();
                        s.version += 1;
                    } else if (b'1' as u16..=b'6' as u16).contains(&key) {
                        s.color = (key - b'1' as u16) as usize;
                    } else if let Some(t) = DrawTool::ALL.into_iter().find(|t| t.key() == key) {
                        s.tool = t;
                    }
                    s.dirty = true;
                }
            });
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Always show a crosshair over the window; returning 1 stops further processing.
            // SAFETY: loading a system cursor and setting it on this window's thread.
            if let Ok(c) = unsafe { LoadCursorW(None, IDC_CROSS) } {
                unsafe { SetCursor(Some(c)) };
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            // SAFETY: stopping our own timer and ending this thread's message loop.
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: forwarding the unmodified message to the default window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Flags the window for repainting on the next timer tick.
fn mark_dirty(l: &mut Local) {
    if let Ok(mut s) = l.shared.lock() {
        s.dirty = true;
    }
}

/// Timer handler: applies changes from the app and repaints when something changed.
///
/// Closes the window when asked to quit (or when the shared state is poisoned), takes over a
/// new position, and switches the window between click-through and input-taking styles when
/// drawing mode changes.
fn tick(l: &mut Local) {
    // Take what is needed from the shared state in one short lock.
    let (quit, drawing, dirty) = match l.shared.lock() {
        Ok(mut s) => {
            let d = s.dirty;
            s.dirty = false;
            if let Some((x, y)) = s.pos.take() {
                (l.x, l.y) = (x, y);
            }
            (s.quit, s.drawing, d)
        }
        Err(_) => (true, false, false),
    };
    if quit {
        // SAFETY: destroying our own window on its thread; WM_DESTROY ends the message loop.
        unsafe {
            let _ = DestroyWindow(l.hwnd);
        }
        return;
    }
    if drawing != l.applied {
        l.applied = drawing;
        DRAWING.store(drawing, Ordering::Relaxed);
        // WS_EX_TRANSPARENT and WS_EX_NOACTIVATE make the window ignore the mouse and never
        // take focus; remove them while drawing and bring the window to the foreground so it
        // also receives keyboard input.
        // SAFETY: reading and writing the extended style of our own window.
        unsafe {
            let style = GetWindowLongPtrW(l.hwnd, GWL_EXSTYLE);
            let passive = (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE).0 as isize;
            let style = if drawing {
                style & !passive
            } else {
                style | passive
            };
            SetWindowLongPtrW(l.hwnd, GWL_EXSTYLE, style);
            if drawing {
                let _ = SetForegroundWindow(l.hwnd);
            }
        }
        // Ending drawing mode mid-drag discards the unfinished shape.
        if !drawing {
            l.current = None;
        }
    }
    if dirty {
        paint(l);
    }
    // Publish at most every 40 ms (25 times per second): copying the whole canvas is costly
    // and window recordings rarely run faster.
    if l.layer_stale
        && LAYER_WANTED.load(Ordering::Relaxed)
        && l.last_layer.elapsed() >= Duration::from_millis(40)
    {
        publish_layer(l);
    }
}

/// Copies the canvas to [`LAYER`] for window recordings, or clears it when nothing is drawn.
fn publish_layer(l: &mut Local) {
    l.layer_stale = false;
    l.last_layer = Instant::now();
    let any = l.current.is_some() || l.base.as_ref().is_some_and(|b| b.has_shapes);
    let layer = match (&l.canvas, any) {
        (Some(c), true) => Some(Arc::new((c.width(), c.height(), c.data().to_vec()))),
        _ => None,
    };
    *LAYER.lock().unwrap_or_else(|e| e.into_inner()) = layer;
}

/// The area a shape covers, padded for line width, arrow heads and anti-aliasing, and clipped
/// to the `w` x `h` window. `None` if it lies outside the window.
fn shape_rect(s: &Shape, scale: f32, w: i32, h: i32) -> Option<PxRect> {
    // 24 px (scaled) covers half the 20 px highlighter width and the arrow head; the extra
    // 3 px covers anti-aliasing and rounding.
    let pad = 24.0 * scale + 3.0;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in &s.points {
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    let r = (
        ((x0 - pad).floor() as i32).max(0),
        ((y0 - pad).floor() as i32).max(0),
        ((x1 + pad).ceil() as i32).min(w),
        ((y1 + pad).ceil() as i32).min(h),
    );
    (r.0 < r.2 && r.1 < r.3).then_some(r)
}

/// The smallest rectangle containing both `a` and `b`.
fn union_rect(a: Option<PxRect>, b: Option<PxRect>) -> Option<PxRect> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Copies rectangle `r` between two images `w` pixels wide (4 bytes per pixel).
fn copy_rect(src: &[u8], dst: &mut [u8], w: i32, r: PxRect) {
    let stride = w as usize * 4;
    let (a, b) = (r.0 as usize * 4, r.2 as usize * 4);
    for y in r.1 as usize..r.3 as usize {
        let row = y * stride;
        dst[row + a..row + b].copy_from_slice(&src[row + a..row + b]);
    }
}

/// Renders all shapes (plus the one in progress) and updates the layered window with them.
///
/// The finished shapes are re-rendered only when they change; while a shape is being drawn,
/// only the area it covers is redrawn and sent to the window. [`tick`] copies the result to
/// [`LAYER`] for window recordings.
fn paint(l: &mut Local) {
    l.last_paint = Instant::now();
    let (w, h) = (l.w.max(1), l.h.max(1));
    let (key, shapes) = match l.shared.lock() {
        Ok(s) => {
            let key: BaseKey = (s.version, s.drawing, s.hole);
            // Copy the shapes only when the cached image is out of date.
            let stale = l.base.as_ref().is_none_or(|b| b.key != key);
            (key, stale.then(|| s.shapes.clone()))
        }
        Err(_) => return,
    };
    let cur_rect = l
        .current
        .as_ref()
        .and_then(|c| shape_rect(c, l.scale, w, h));
    // SAFETY: `bits` points to the DIB's w * h * 4 bytes and the DIB lives until the window
    // thread ends.
    let out = unsafe { std::slice::from_raw_parts_mut(l.bits, w as usize * h as usize * 4) };

    // Rebuild the finished shapes when they, the drawing mode or the control bar area changed.
    if let Some(shapes) = shapes {
        let Some(mut pixels) = Pixmap::new(w as u32, h as u32) else {
            return;
        };
        let (_, drawing, hole) = key;
        if drawing {
            // A layered window only receives the mouse over pixels with non-zero alpha, so fill
            // with an invisible alpha-1 tint and leave the control bar fully transparent.
            pixels.fill(Color::from_rgba8(0, 0, 0, 1));
            if let Some((hx, hy, hw, hh)) = hole {
                let r = (hx.max(0), hy.max(0), (hx + hw).min(w), (hy + hh).min(h));
                if r.0 < r.2 && r.1 < r.3 {
                    let stride = w as usize * 4;
                    let data = pixels.data_mut();
                    for y in r.1 as usize..r.3 as usize {
                        data[y * stride + r.0 as usize * 4..y * stride + r.2 as usize * 4].fill(0);
                    }
                }
            }
        }
        for s in &shapes {
            draw_shape(&mut pixels, s, l.scale);
        }
        l.base = Some(Base {
            key,
            has_shapes: !shapes.is_empty(),
            pixels,
        });
        l.canvas = None;
    }
    let Some(base) = &l.base else { return };

    let dirty = if let Some(canvas) = &mut l.canvas {
        // Only the area of the shape in progress (now and at the last repaint) changed.
        let dirty = union_rect(l.prev_rect, cur_rect);
        if let Some(r) = dirty {
            copy_rect(base.pixels.data(), canvas.data_mut(), w, r);
            if let Some(c) = &l.current {
                draw_shape(canvas, c, l.scale);
            }
            copy_rect(canvas.data(), out, w, r);
        }
        dirty
    } else {
        // Everything changed: start the canvas over from the finished shapes.
        let mut canvas = base.pixels.clone();
        if let Some(c) = &l.current {
            draw_shape(&mut canvas, c, l.scale);
        }
        out.copy_from_slice(canvas.data());
        l.canvas = Some(canvas);
        None
    };
    l.prev_rect = cur_rect;
    l.layer_stale = true;

    // Push the bitmap to the window with per-pixel alpha (only the changed area when known),
    // also applying any new position.
    // SAFETY: all handles are valid for the lifetime of the window; the pointers in `info`
    // refer to locals that outlive the call; the screen DC is released.
    unsafe {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x: l.x, y: l.y };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let rect = dirty.map(|r| RECT {
            left: r.0,
            top: r.1,
            right: r.2,
            bottom: r.3,
        });
        let screen = GetDC(None);
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: screen,
            pptDst: &dst,
            psize: &size,
            hdcSrc: l.mem,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: rect.as_ref().map_or(std::ptr::null(), |r| r as *const RECT),
        };
        let _ = UpdateLayeredWindowIndirect(l.hwnd, &info);
        ReleaseDC(None, screen);
    }
}

/// Draws one shape into `pm` (BGRA, see [`Base`]); line widths are multiplied by the DPI
/// `scale`.
///
/// Arrows, rectangles and ellipses smaller than about 3 pixels are skipped so that a plain
/// click does not leave a speck.
fn draw_shape(pm: &mut Pixmap, s: &Shape, scale: f32) {
    // Red and blue swapped: the window's pixels are BGRA, and this avoids converting them.
    let [b, g, r] = s.color;
    // First and last point: the drag's start and end for the two-point tools.
    let (a, b2) = (s.points[0], *s.points.last().unwrap_or(&s.points[0]));
    match s.tool {
        DrawTool::Pen | DrawTool::Highlighter => {
            // Freehand polyline with round caps and joins: 4 px opaque for the pen,
            // 20 px translucent for the highlighter.
            let highlighter = s.tool == DrawTool::Highlighter;
            let mut pb = PathBuilder::new();
            pb.move_to(a.0, a.1);
            let pts = &s.points;
            if pts.len() == 2 && a == b2 {
                pb.line_to(a.0 + 0.1, a.1 + 0.1); // a dot
            } else if pts.len() <= 2 {
                pb.line_to(b2.0, b2.1);
            } else {
                // Smooth curve: each sampled point is the control point of a quadratic curve
                // between the midpoints of its neighbouring segments, so the stroke has no
                // corners where the mouse samples are far apart.
                for seg in pts[1..].windows(2) {
                    let (p, q) = (seg[0], seg[1]);
                    pb.quad_to(p.0, p.1, (p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
                }
                pb.line_to(b2.0, b2.1);
            }
            let Some(path) = pb.finish() else { return };
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, if highlighter { 110 } else { 255 });
            paint.anti_alias = true;
            let stroke = Stroke {
                width: if highlighter { 20.0 } else { 4.0 } * scale,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
        DrawTool::Arrow => {
            if (a.0 - b2.0).abs() + (a.1 - b2.1).abs() > 3.0 {
                crate::draw::draw_arrow(pm, a.0, a.1, b2.0, b2.1, 5.0 * scale, [r, g, b, 255]);
            }
        }
        DrawTool::Rectangle | DrawTool::Ellipse => {
            // Normalise the drag into a rectangle whatever direction it went.
            let (x, y) = (a.0.min(b2.0), a.1.min(b2.1));
            let (w, h) = ((a.0 - b2.0).abs(), (a.1 - b2.1).abs());
            if w + h > 3.0 {
                if s.tool == DrawTool::Rectangle {
                    crate::draw::stroke_rect(pm, x, y, w, h, 4.0 * scale, [r, g, b, 255]);
                } else {
                    crate::draw::stroke_ellipse(pm, x, y, w, h, 4.0 * scale, [r, g, b, 255]);
                }
            }
        }
    }
}
