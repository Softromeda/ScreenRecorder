// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Softromeda Screen Recorder: a screen recorder and video editor for Windows 10 and 11.
//!
//! It uses only components built into Windows: DXGI Desktop Duplication for capture, Media
//! Foundation for H.264 / AAC / MP3 encoding and decoding, and WASAPI for audio.
//!
//! This file declares the modules and contains the entry point, which handles the command-line
//! modes (`--record`, `--list-devices`, `--selftest`, …; see [`cli`]) and otherwise starts the
//! egui application ([`app::App`]) with the capture panel as its main window.

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]
// Most helpers are only used by the Windows application; avoid dead-code noise elsewhere.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

// Platform-independent modules (they also build and are unit-tested on other systems).
mod clock;
mod draw;
mod editor;
mod geometry;
mod hotkeys;
mod i18n;
mod job;
mod media;
mod settings;
mod theme;
mod update;
mod util;

// The application itself is Windows-only.
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod compose;
#[cfg(windows)]
mod library;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod recorder;
#[cfg(windows)]
mod selector;
#[cfg(windows)]
mod tools;
#[cfg(windows)]
mod tray;

/// Entry point on unsupported platforms: prints a message and exits with an error code.
#[cfg(not(windows))]
fn main() {
    eprintln!("Softromeda Screen Recorder runs on Windows only.");
    std::process::exit(1);
}

/// Command-line usage text printed for `--help` / `-h`.
#[cfg(windows)]
const HELP: &str = "Softromeda Screen Recorder - screen recorder and video editor

USAGE:
    softromeda-screen-recorder [VIDEO]              Start the GUI (optionally open VIDEO in the editor)
    softromeda-screen-recorder --record [OPTIONS]   Record without the GUI, then exit
    softromeda-screen-recorder --list-devices       List monitors, audio devices and webcams
    softromeda-screen-recorder --selftest           Check screen capture, encoding and decoding

RECORD OPTIONS:
    --duration <SECONDS>    Stop after this many seconds (otherwise press Enter to stop)
    --region <X,Y,W,H>      Area of the monitor to record (default: full screen)
    --monitor <INDEX>       Monitor index (default: primary)
    --format <FMT>          mp4 | gif | mp3 (default: from settings)
    --fps <N>               Frames per second
    --mic / --speakers      Record microphone / system audio
    --webcam                Include the webcam
    --output <FILE>         Output file (default: automatic name in the output folder)
";

/// Entry point on Windows.
///
/// Sets up DPI awareness, data migration and logging, then either runs one of the command-line
/// modes and exits, or opens the GUI. A path given as a plain argument is picked up by
/// [`app::App::new`] and opened in the video editor.
#[cfg(windows)]
fn main() -> eframe::Result<()> {
    // Work in physical pixels everywhere: DXGI capture always reports physical coordinates, so
    // per-monitor DPI awareness keeps window, selection and capture rectangles consistent.
    // SAFETY: a plain Win32 call with a predefined constant, made before any window exists.
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    // Move data from folders used under earlier program names before anything reads settings.
    util::migrate_old_data();
    util::init_logging();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // Release builds use the GUI subsystem and have no console of their own: attach to the
    // console of the parent process so command-line output appears in the terminal that ran us.
    if args.iter().any(|a| a.starts_with("--") || a == "-h") {
        // SAFETY: AttachConsole has no memory-safety preconditions; failure (no parent console)
        // is harmless and ignored.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(());
    }
    // Command-line modes: each runs to completion and exits without opening the GUI.
    if args.iter().any(|a| a == "--selftest") {
        let ok = cli::selftest();
        std::process::exit(if ok { 0 } else { 1 });
    }
    // Debug builds only: open the on-screen drawing overlay by itself for 40 s, for testing.
    #[cfg(debug_assertions)]
    if args.iter().any(|a| a == "--draw-test") {
        let o = overlay::DrawOverlay::start(100, 100, 900, 600);
        o.set_drawing(true);
        std::thread::sleep(std::time::Duration::from_secs(40));
        return Ok(());
    }
    if args.iter().any(|a| a == "--list-devices") {
        cli::list_devices();
        return Ok(());
    }
    if args.iter().any(|a| a == "--record") {
        if let Err(e) = cli::record(&args) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Open the capture panel in the centre of the primary screen, sized for the panel mode
    // (compact or detailed) stored in the settings.
    let size = app::panel_size(settings::Settings::load().detailed_mode);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(util::APP_NAME)
        .with_app_id("softromeda-screen-recorder")
        .with_inner_size(size)
        .with_min_inner_size([300.0, 36.0])
        // The capture panel is a compact dialog whose size the app controls: maximising or
        // resizing it would only leave empty space.
        .with_maximize_button(false)
        .with_resizable(false)
        .with_icon(util::app_icon());
    // `--tray` is passed when started with Windows: begin hidden in the notification area,
    // provided the tray icon is enabled (otherwise the window could not be reached).
    if args.iter().any(|a| a == "--tray") && settings::Settings::load().tray_icon {
        viewport = viewport.with_visible(false);
    }
    let position = geometry::centered_on_primary(size.into());
    log::debug!("initial window position: {position:?}");
    if let Some(pos) = position {
        viewport = viewport.with_position(pos);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Let eframe centre the window when the monitor layout could not be read.
        centered: position.is_none(),
        ..Default::default()
    };
    eframe::run_native(
        util::APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
