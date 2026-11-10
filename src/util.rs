// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Small cross-cutting helpers used by several modules.
//!
//! Covers opening files and folders in File Explorer, putting the computer to sleep after a
//! scheduled recording, the program icon and name, theme-aware status colours, per-user data
//! folders (including moving data out of legacy folder names), logging to a file, the
//! "reveal when ready" logic that shows secondary egui windows only once their layout has
//! settled, and hiding the desktop icons during a recording.

use std::path::Path;
use std::process::Command;

/// Configures `cmd` so that starting it does not flash a console window (Windows only).
#[cfg(windows)]
fn hidden(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Non-Windows stand-in for the Windows `hidden`: returns the command unchanged.
#[cfg(not(windows))]
fn hidden(cmd: Command) -> Command {
    cmd
}

/// Opens a file or folder with its default application via File Explorer.
///
/// Failures to start Explorer are logged, not returned.
#[allow(dead_code)]
pub fn open_path(path: &Path) {
    let mut cmd = Command::new("explorer");
    cmd.arg(path);
    if let Err(e) = hidden(cmd).spawn() {
        log::warn!("could not open {}: {e}", path.display());
    }
}

/// Opens File Explorer on the folder containing `path`, with that file selected.
#[allow(dead_code)]
pub fn reveal_in_folder(path: &Path) {
    let mut cmd = Command::new("explorer");
    cmd.arg(format!("/select,{}", path.display()));
    let _ = hidden(cmd).spawn();
}

/// Puts the computer to sleep; used when a scheduled recording asks for it on completion.
///
/// Calls `SetSuspendState` through `rundll32`. Failures are logged, not returned.
#[allow(dead_code)]
pub fn sleep_computer() {
    let mut cmd = Command::new("rundll32.exe");
    // Arguments: hibernate = 0 (sleep, not hibernate), force = 1, disable wake events = 0.
    cmd.args(["powrprof.dll,SetSuspendState", "0,1,0"]);
    if let Err(e) = hidden(cmd).spawn() {
        log::warn!("could not put the computer to sleep: {e}");
    }
}

/// Returns the program icon for egui windows, decoded once and then shared.
///
/// This is the window icon; the Explorer icon of the .exe is `assets/icon.ico`, embedded as a
/// resource by `build.rs`, and the two files should show the same artwork.
///
/// # Panics
/// Panics if the bundled icon cannot be decoded, which would be a build error.
pub fn app_icon() -> std::sync::Arc<egui::IconData> {
    static ICON: std::sync::OnceLock<std::sync::Arc<egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
            .expect("bundled icon")
            .to_rgba8();
        std::sync::Arc::new(egui::IconData {
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
        })
    })
    .clone()
}

/// Green used for success and "on" states, chosen to be readable in both light and dark themes.
pub fn ok_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(102, 187, 106)
    } else {
        egui::Color32::from_rgb(27, 120, 40)
    }
}

/// Red used for errors, taken from the active egui theme.
pub fn error_color(ui: &egui::Ui) -> egui::Color32 {
    ui.visuals().error_fg_color
}

/// The program's name as shown to users (window titles, tray tooltip, log header).
pub const APP_NAME: &str = "Softromeda Screen Recorder";

/// Returns the standard per-user folders for the program (under `%APPDATA%\Softromeda\Recorder`
/// and `%LOCALAPPDATA%\Softromeda\Recorder` on Windows), or `None` if no home folder is known.
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("org", "Softromeda", "Recorder")
}

/// Moves settings, logs and unfinished recordings from the legacy folder names
/// ("Capture", "ScreenRecorder") to the current per-user folders.
///
/// Called once at start-up. Nothing is moved if the destination already exists, so existing
/// data is never overwritten.
pub fn migrate_old_data() {
    for name in ["Capture", "ScreenRecorder"] {
        if let (Some(old), Some(new)) = (
            directories::ProjectDirs::from("org", "Softromeda", name),
            project_dirs(),
        ) {
            migrate_dirs(&old, &new);
        }
    }
}

/// Moves the configuration and local-data folders of `old` to those of `new`.
///
/// Each folder is renamed only when the source exists and the destination does not; errors are
/// logged and otherwise ignored.
fn migrate_dirs(old: &directories::ProjectDirs, new: &directories::ProjectDirs) {
    let pairs = [
        (
            old.config_dir().to_path_buf(),
            new.config_dir().to_path_buf(),
        ),
        (
            old.data_local_dir().to_path_buf(),
            new.data_local_dir().to_path_buf(),
        ),
    ];
    for (from, to) in pairs {
        // On Windows these are `<project>\config` and `<project>\data`: when the last
        // component matches, move the whole project folder so nothing is left behind.
        let (from, to) = match (from.parent(), to.parent()) {
            (Some(f), Some(t)) if from.file_name() == to.file_name() => {
                (f.to_path_buf(), t.to_path_buf())
            }
            _ => (from, to),
        };
        // Never overwrite existing data; create the destination's parent so rename can work.
        if from.exists() && !to.exists() {
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => log::info!("moved {} to {}", from.display(), to.display()),
                Err(e) => log::warn!("could not move {}: {e}", from.display()),
            }
        }
    }
}

/// Returns the folder for logs and other local app data, falling back to a folder in the
/// temporary directory when no per-user folder is available.
pub fn data_dir() -> std::path::PathBuf {
    project_dirs()
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("softromeda-recorder"))
}

/// Returns the path of the current log file inside [`data_dir`].
pub fn log_path() -> std::path::PathBuf {
    data_dir().join("softromeda-screen-recorder.log")
}

/// Log writer that duplicates every line to stderr and, when available, to the log file.
struct Tee(Option<std::fs::File>);

/// Write errors are ignored so that logging can never fail the caller.
impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        if let Some(f) = &mut self.0 {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = &mut self.0 {
            f.flush()?;
        }
        Ok(())
    }
}

/// Initialises logging to `softromeda-screen-recorder.log` and stderr, and logs panics.
///
/// The log of the previous run is kept as `softromeda-screen-recorder.old.log`. A file is needed
/// because release builds on Windows have no console. The default filter is `warn` for
/// dependencies and `info` for this program; `RUST_LOG` overrides it. Must be called once,
/// before any other logging.
pub fn init_logging() {
    // Rotate the previous log and open a fresh file; logging continues to stderr only if
    // the folder or file cannot be created.
    let path = log_path();
    let file = path.parent().and_then(|dir| {
        std::fs::create_dir_all(dir).ok()?;
        let _ = std::fs::rename(&path, dir.join("softromeda-screen-recorder.old.log"));
        std::fs::File::create(&path).ok()
    });
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,softromeda_screen_recorder=info"),
    )
    .target(env_logger::Target::Pipe(Box::new(Tee(file))))
    .init();
    log::info!(
        "{} {} on {} {}",
        APP_NAME,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // Log panics to the file before running the default hook (which prints to stderr).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
}

/// Key in egui's temporary data under which [`reveal_when_ready`] counts frames per viewport.
fn reveal_key(viewport: egui::ViewportId) -> egui::Id {
    egui::Id::unique(("reveal-frames", viewport))
}

/// Shows a secondary window once its layout has settled; call at the start of its UI each frame.
///
/// The window is created hidden (`with_visible(false)`). This function counts frames for the
/// current viewport, requests repaints until three frames have run, then makes the window
/// visible and focuses it. This avoids a flash of the window at the wrong size or position on
/// high-DPI screens. Call [`forget_reveal`] when the window closes so that it starts hidden
/// again the next time it opens.
pub fn reveal_when_ready(ctx: &egui::Context) {
    let key = reveal_key(ctx.viewport_id());
    // Count up to 3 frames; 4 marks a window that has already been revealed.
    let frames = ctx.data_mut(|d| {
        let n = d.get_temp_mut_or_default::<u32>(key);
        if *n < 3 {
            *n += 1;
        }
        *n
    });
    match frames {
        3 => {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.data_mut(|d| d.insert_temp(key, 4u32));
        }
        // Keep frames coming while the window is still hidden (it receives no input events).
        n if n < 3 => ctx.request_repaint(),
        _ => {}
    }
}

/// Resets the [`reveal_when_ready`] frame counter of a closed window's `viewport`.
pub fn forget_reveal(ctx: &egui::Context, viewport: egui::ViewportId) {
    ctx.data_mut(|d| d.remove::<u32>(reveal_key(viewport)));
}

/// Shows or hides the desktop icons. Returns whether the icons were found.
///
/// The icons are a list view inside the shell's `SHELLDLL_DefView` window, which is a child of
/// `Progman` or, while a slideshow or animated wallpaper is active, of one of the `WorkerW`
/// windows.
#[cfg(windows)]
pub fn set_desktop_icons_visible(visible: bool) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, SW_HIDE, SW_SHOW, ShowWindow,
    };
    use windows::core::w;
    // SAFETY: only finds windows by class name and changes their visibility.
    unsafe {
        let view_under =
            |parent: HWND| FindWindowExW(Some(parent), None, w!("SHELLDLL_DefView"), None).ok();
        let mut view = FindWindowW(w!("Progman"), None).ok().and_then(view_under);
        // Otherwise look through the WorkerW windows.
        let mut worker: Option<HWND> = None;
        while view.is_none() {
            match FindWindowExW(None, worker, w!("WorkerW"), None) {
                Ok(w) => {
                    view = view_under(w);
                    worker = Some(w);
                }
                Err(_) => break,
            }
        }
        let Some(list) =
            view.and_then(|v| FindWindowExW(Some(v), None, w!("SysListView32"), None).ok())
        else {
            return false;
        };
        let _ = ShowWindow(list, if visible { SW_SHOW } else { SW_HIDE });
        true
    }
}
