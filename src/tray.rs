// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Notification-area (system tray) icon and the "start with Windows" setting.
//!
//! [`Tray::start`] creates the icon on its own thread, which owns a hidden message-only style
//! window and runs a plain Win32 message loop. Clicking the icon shows the main window; its
//! context menu offers Show / Start recording (or Pause / Stop and save while recording) /
//! Recordings / Options / Exit. Choices are sent to the UI thread as [`TrayCommand`]s over a
//! channel and collected with [`Tray::poll`]. [`set_start_with_windows`] adds or removes the
//! per-user `Run` registry entry that launches the program into the tray at sign-in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// An action chosen from the tray icon, handled by the app on the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Show and focus the main window (left click, double click or menu item).
    Show,
    /// Start a new recording with the current capture settings.
    Record,
    /// Pause the running recording, or resume it if paused.
    PauseResume,
    /// Stop the running recording and save it.
    Stop,
    /// Open the recordings library.
    Recordings,
    /// Open the Options window.
    Options,
    /// Quit the program.
    Exit,
}

/// Callback message the shell sends to the tray window for mouse events on the icon.
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by [`Tray::set_state`] to make the tray thread update the tooltip.
const WM_REFRESH: u32 = WM_APP + 2;

// Context-menu command ids; 0 is reserved for separators and "no choice".
/// Menu id of "Show window" ([`TrayCommand::Show`]).
const ID_SHOW: usize = 1;
/// Menu id of "Start recording" ([`TrayCommand::Record`]).
const ID_RECORD: usize = 2;
/// Menu id of "Pause" / "Resume" ([`TrayCommand::PauseResume`]).
const ID_PAUSE: usize = 3;
/// Menu id of "Stop and save" ([`TrayCommand::Stop`]).
const ID_STOP: usize = 4;
/// Menu id of "Recordings" ([`TrayCommand::Recordings`]).
const ID_RECORDINGS: usize = 5;
/// Menu id of "Options" ([`TrayCommand::Options`]).
const ID_OPTIONS: usize = 6;
/// Menu id of "Exit" ([`TrayCommand::Exit`]).
const ID_EXIT: usize = 7;

/// State shared between the UI thread and the tray thread.
struct Shared {
    /// Sends chosen commands to the UI thread.
    tx: Sender<TrayCommand>,
    /// Used to wake the UI thread so that it polls the commands promptly.
    ctx: egui::Context,
    /// Whether a recording is running; selects the menu items and tooltip.
    recording: AtomicBool,
    /// Whether the running recording is paused; selects "Pause" or "Resume".
    paused: AtomicBool,
}

/// Handle to the tray icon owned by the app; dropping it removes the icon and ends the thread.
pub struct Tray {
    /// Receives the commands chosen in the tray.
    rx: Receiver<TrayCommand>,
    /// State shared with the tray thread.
    shared: Arc<Shared>,
    /// The tray window's `HWND` as an integer; 0 until the window exists and after it closes.
    hwnd: Arc<AtomicIsize>,
    /// The tray thread, joined on drop; `None` if it could not be spawned.
    thread: Option<JoinHandle<()>>,
}

impl Tray {
    /// Spawns the tray thread, which creates the icon; returns immediately.
    ///
    /// If the icon cannot be created the error is logged and the program runs without one.
    pub fn start(ctx: &egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            tx,
            ctx: ctx.clone(),
            recording: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        });
        let hwnd = Arc::new(AtomicIsize::new(0));
        let (s, h) = (shared.clone(), hwnd.clone());
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                if let Err(e) = run(s, h) {
                    log::warn!("tray icon: {e:#}");
                }
            })
            .ok();
        Self {
            rx,
            shared,
            hwnd,
            thread,
        }
    }

    /// Returns the commands chosen in the tray since the last call, oldest first.
    pub fn poll(&self) -> Vec<TrayCommand> {
        self.rx.try_iter().collect()
    }

    /// Tells the tray the recording state, which drives the menu items and the tooltip.
    ///
    /// Called by the app every frame; the tray window is only asked to refresh when the
    /// comparison detects a change, to avoid posting a message per frame.
    pub fn set_state(&self, recording: bool, paused: bool) {
        // Store both new values first, then compare.
        let recording_changed =
            self.shared.recording.swap(recording, Ordering::Relaxed) != recording;
        let paused_changed = self.shared.paused.swap(paused, Ordering::Relaxed) != paused;
        let changed = recording_changed || paused_changed;
        if changed {
            self.post(WM_REFRESH);
        }
    }

    /// Posts `msg` to the tray window, if it exists. Safe to call from any thread.
    fn post(&self, msg: u32) {
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: PostMessageW only queues a message; a stale handle makes it fail, which
            // is ignored.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(h as *mut core::ffi::c_void)),
                    msg,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

/// Closes the tray window (which removes the icon and ends the message loop) and waits for
/// the tray thread to finish.
impl Drop for Tray {
    fn drop(&mut self) {
        self.post(WM_CLOSE);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// Tray-thread state reachable from the window procedure, which receives no user data.
thread_local! {
    /// The shared state, set once the tray window exists.
    static SHARED: std::cell::RefCell<Option<Arc<Shared>>> = const { std::cell::RefCell::new(None) };
    /// Id of the registered "TaskbarCreated" message (0 until registered).
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Returns the icon's tooltip: the program name, plus "Recording…" while recording.
fn tooltip() -> String {
    let recording = SHARED.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| s.recording.load(Ordering::Relaxed))
            .unwrap_or(false)
    });
    if recording {
        format!(
            "{} — {}",
            crate::util::APP_NAME,
            crate::i18n::tr("Recording…")
        )
    } else {
        crate::util::APP_NAME.to_string()
    }
}

/// Builds the `NOTIFYICONDATAW` for the tray window's icon with the current tooltip.
///
/// `add` loads the icon image as well; it can be `false` when deleting the icon.
fn icon_data(hwnd: HWND, add: bool) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    if add {
        nid.hIcon = load_icon();
    }
    // Copy the tooltip into the fixed-size buffer, truncated so a NUL terminator remains.
    let tip: Vec<u16> = tooltip().encode_utf16().collect();
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    nid
}

/// Loads the program icon (resource 1, embedded by `build.rs`) at the small-icon size for this
/// display, falling back to the generic application icon.
fn load_icon() -> HICON {
    // SAFETY: plain Win32 calls with valid arguments; resource id 1 is passed as an integer
    // resource pointer (MAKEINTRESOURCE), which LoadImageW expects.
    unsafe {
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        GetModuleHandleW(None)
            .ok()
            .and_then(|m| {
                LoadImageW(
                    Some(m.into()),
                    PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    cx,
                    cy,
                    LR_DEFAULTCOLOR,
                )
                .ok()
            })
            .map(|h| HICON(h.0))
            .or_else(|| LoadIconW(None, IDI_APPLICATION).ok())
            .unwrap_or_default()
    }
}

/// Adds the icon to the notification area for the tray window `hwnd`.
fn add_icon(hwnd: HWND) {
    // SAFETY: the NOTIFYICONDATAW is fully initialised and outlives the call.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd, true));
    }
}

/// Body of the tray thread: creates the hidden tray window and icon, then runs the message
/// loop until the window is destroyed.
///
/// Publishes the window handle through `hwnd_out` while the loop runs so that [`Tray::post`]
/// can reach it. Fails if the window cannot be created.
fn run(shared: Arc<Shared>, hwnd_out: Arc<AtomicIsize>) -> anyhow::Result<()> {
    // SAFETY: standard window creation and message loop on this thread; the class name and
    // title are static wide strings.
    unsafe {
        // Register a window class and create a hidden, zero-size window to receive messages.
        let hinst = GetModuleHandleW(None)?;
        let class = w!("SoftromedaRecorderTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Softromeda Screen Recorder tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        SHARED.with(|s| *s.borrow_mut() = Some(shared));
        // Windows broadcasts this message when Explorer (and so the taskbar) restarts; the
        // icon must then be added again.
        TASKBAR_CREATED.with(|t| t.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        add_icon(hwnd);
        hwnd_out.store(hwnd.0 as isize, Ordering::Relaxed);

        // Message loop; ends when WM_DESTROY posts WM_QUIT.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        hwnd_out.store(0, Ordering::Relaxed);
    }
    Ok(())
}

/// Sends `cmd` to the UI thread and wakes it with a repaint request.
fn send(cmd: TrayCommand) {
    SHARED.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            let _ = s.tx.send(cmd);
            s.ctx.request_repaint();
        }
    });
}

/// Window procedure of the hidden tray window.
///
/// Handles icon mouse events, tooltip refreshes, closing (which removes the icon and ends the
/// message loop) and re-adding the icon after an Explorer restart.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // The low word of `lp` holds the mouse message that occurred on the icon.
            match (lp.0 & 0xffff) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(TrayCommand::Show),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_REFRESH => {
            // Re-send the icon data so that the shell picks up the new tooltip.
            // SAFETY: the NOTIFYICONDATAW is fully initialised and outlives the call.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &icon_data(hwnd, true));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window on its thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Remove the icon so that no stale icon remains, then end the message loop.
            // SAFETY: as for WM_REFRESH; PostQuitMessage only affects this thread's queue.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd, false));
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => {
            // The TaskbarCreated id is assigned at run time, so it cannot be a match arm.
            if msg != 0 && msg == TASKBAR_CREATED.with(|t| t.get()) {
                add_icon(hwnd);
                return LRESULT(0);
            }
            // SAFETY: forwarding the unmodified message to the default window procedure.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

/// Shows the tray context menu at the cursor and sends the chosen command, if any.
///
/// The items depend on the recording state: "Start recording" when idle, "Pause"/"Resume" and
/// "Stop and save" while recording. Runs a modal menu loop on the tray thread.
fn show_menu(hwnd: HWND) {
    use crate::i18n::tr;
    let (recording, paused) = SHARED.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                (
                    s.recording.load(Ordering::Relaxed),
                    s.paused.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((false, false))
    });
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    // SAFETY: the menu handle is created and destroyed here; the UTF-16 item texts in `texts`
    // stay alive until AppendMenuW has copied them.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        // Menu items as (command id, label); id 0 marks a separator.
        let mut items: Vec<(usize, &str)> = vec![(ID_SHOW, tr("Show window"))];
        items.push((0, ""));
        if recording {
            items.push((ID_PAUSE, if paused { tr("Resume") } else { tr("Pause") }));
            items.push((ID_STOP, tr("Stop and save")));
        } else {
            items.push((ID_RECORD, tr("Start recording")));
        }
        items.push((0, ""));
        items.push((ID_RECORDINGS, tr("Recordings")));
        items.push((ID_OPTIONS, tr("Options")));
        items.push((0, ""));
        items.push((ID_EXIT, tr("Exit")));
        let texts: Vec<Vec<u16>> = items.iter().map(|(_, t)| wide(t)).collect();
        for ((id, _), text) in items.iter().zip(&texts) {
            if *id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let _ = AppendMenuW(menu, MF_STRING, *id, PCWSTR(text.as_ptr()));
            }
        }
        // Show the default item in bold, matching what a left click does.
        let _ = SetMenuDefaultItem(menu, ID_SHOW as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // The window must be in the foreground, or the menu does not close when the user
        // clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        // TPM_RETURNCMD returns the chosen id directly instead of posting WM_COMMAND.
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        // Documented workaround (see the TrackPopupMenu remarks): posting a dummy message
        // makes the menu close properly so that the next one opens reliably.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        let cmd = match chosen.0 as usize {
            ID_SHOW => Some(TrayCommand::Show),
            ID_RECORD => Some(TrayCommand::Record),
            ID_PAUSE => Some(TrayCommand::PauseResume),
            ID_STOP => Some(TrayCommand::Stop),
            ID_RECORDINGS => Some(TrayCommand::Recordings),
            ID_OPTIONS => Some(TrayCommand::Options),
            ID_EXIT => Some(TrayCommand::Exit),
            _ => None,
        };
        if let Some(c) = cmd {
            send(c);
        }
    }
}

// ----------------------------------------------------------------------
// Start with Windows

/// Per-user registry key whose values are run when the user signs in.
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// Name of this program's value under [`RUN_KEY`].
///
/// The value deliberately keeps the product's former name, "Softromeda Recorder", so that
/// "Start with Windows" entries written by earlier versions are still found and can be updated
/// or removed. The name is not shown to users, so it does not need to match the current one.
const RUN_VALUE: PCWSTR = w!("Softromeda Recorder");

/// Enables or disables starting the program, minimised to the tray, when the user signs in.
///
/// Enabling writes `"<path to exe>" --tray` to the `HKCU\...\Run` key; disabling deletes the
/// value. Errors are logged (enable) or ignored (disable, where the value may not exist).
pub fn set_start_with_windows(on: bool) {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    // SAFETY: the data pointer and its byte length (including the NUL) describe a live buffer.
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            // Quote the path because it may contain spaces; REG_SZ data is NUL-terminated
            // UTF-16 and its size is given in bytes.
            let cmd = format!("\"{}\" --tray", exe.display());
            let data: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(data.as_ptr() as *const _),
                (data.len() * 2) as u32,
            );
            if r.is_err() {
                log::warn!("could not enable start with Windows: {r:?}");
            }
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}
