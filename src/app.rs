// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! The main application window and its state machine.
//!
//! [`App`] implements [`eframe::App`] and owns everything the user interacts with: the capture
//! panel (choose what to record, devices, quick settings), the recording control bar, the
//! Options, Timer and recovery dialogs, and the windows of the conversion tools, the recordings
//! library and the video editor. A recording moves through the [`Phase`]s
//! idle → selecting → countdown → recording → finalising, and the single native main window
//! changes its [`Layout`] to match (panel, selection overlay, countdown, control bar or hidden).
//! The per-frame logic in [`App::tick`] also runs while the window is hidden, so hotkeys, the
//! tray icon and the timer keep working.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveTime, TimeZone};
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Vec2, ViewportCommand, ViewportId};

use crate::audio;
use crate::compose;
use crate::draw;
use crate::editor::Editor;
use crate::geometry::{self, CaptureRegion, MonitorInfo};
use crate::hotkeys::{HotkeyAction, Hotkeys};
#[allow(unused_imports)]
use crate::i18n::{tf, tr};
use crate::job::{Job, JobStatus};
use crate::media::format_time;
use crate::recorder::{self, FinishedSession, Recorder};
use crate::selector::{SelectMode, Selector, SelectorResult};
use crate::settings::{AfterRecording, OutputFormat, Settings, Theme, WebcamPosition};
use crate::tools::{ToolKind, ToolsWindow};

/// Width of the capture panel in logical points (the height follows the content).
const PANEL_WIDTH: f32 = 760.0;
/// Initial capture panel size with the recording settings hidden.
const PANEL_SIZE: [f32; 2] = [PANEL_WIDTH, 330.0];
/// Initial capture panel size with the recording settings shown.
const PANEL_SIZE_DETAILED: [f32; 2] = [PANEL_WIDTH, 540.0];

/// Returns the initial capture panel size in logical points, for simple or detailed mode.
pub fn panel_size(detailed: bool) -> [f32; 2] {
    if detailed {
        PANEL_SIZE_DETAILED
    } else {
        PANEL_SIZE
    }
}

/// Where the application is in the recording workflow.
///
/// In every phase, `region` is the area to record; `None` means an audio-only recording.
enum Phase {
    /// Nothing is happening; the capture panel accepts new recordings.
    Idle,
    /// The user chose a selection mode; the panel is being hidden before the screenshot for the
    /// selection overlay is taken.
    PendingSelect {
        /// What the overlay will let the user select.
        mode: SelectMode,
        /// When the panel was asked to hide.
        since: Instant,
    },
    /// The full-screen selection overlay is shown and the user is picking a window or area.
    Selecting(Box<Selector>),
    /// The countdown is shown.
    Countdown {
        /// The area to record.
        region: Option<CaptureRegion>,
        /// When the countdown ends and recording starts.
        until: Instant,
    },
    /// Countdown finished; waiting for the countdown window to disappear from the screen so
    /// that it is not captured in the first frames.
    Starting {
        /// The area to record.
        region: Option<CaptureRegion>,
        /// When the recorder is started.
        at: Instant,
    },
    /// A recording is in progress (possibly paused).
    Recording {
        /// The running recorder (boxed to keep [`Phase`] small).
        rec: Box<Recorder>,
        /// The area being recorded.
        region: Option<CaptureRegion>,
    },
    /// The recording (or a recovered session) is being encoded into the output file.
    Finalizing {
        /// The background job writing the output file.
        job: Job,
        /// Put the computer to sleep when the job is done (set by the timer).
        sleep_after: bool,
    },
}

/// What the main window currently is.
///
/// The main window doubles as the region-selection overlay, the countdown and the recording
/// control bar, because new native windows cannot be created while it is hidden. Rectangles are
/// in logical points on the desktop.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Layout {
    /// The normal, decorated capture panel.
    Panel,
    /// The borderless, always-on-top recording control bar.
    Bar,
    /// The window is invisible (selection pending, starting, recording without a bar, in tray).
    Hidden,
    /// A borderless window covering the given monitor rectangle, showing the selection overlay.
    Overlay(Rect),
    /// A small borderless window showing the countdown in the given rectangle.
    Countdown(Rect),
}

/// What a scheduled (timer) recording records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerTarget {
    /// The whole screen under the mouse pointer.
    FullScreen,
    /// The last recorded region, or the full screen if there is none.
    LastRegion,
}

/// State of the scheduled-recording timer and its dialog.
struct TimerState {
    /// Whether the Timer dialog is open.
    open: bool,
    /// Start time as typed by the user (`HH:MM`).
    start: String,
    /// Stop time as typed by the user (`HH:MM`).
    end: String,
    /// What to record when the timer fires.
    target: TimerTarget,
    /// Put the computer to sleep once the scheduled recording has been saved.
    sleep_after: bool,
    /// The armed start and stop instants, or `None` when the timer is not armed.
    armed: Option<(chrono::DateTime<Local>, chrono::DateTime<Local>)>,
    /// Whether the scheduled recording has started and is waiting for the stop time.
    running: bool,
    /// Error shown in the dialog (invalid time, skipped recording).
    error: Option<String>,
}

/// The pages of the Options dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionsTab {
    /// Output folder and file names, log file, appearance, tray icon and text font.
    General,
    /// Format, frame rate, quality, size, countdown and recording-area options.
    Video,
    /// Microphone and speaker devices, volumes and voice processing.
    Audio,
    /// Webcam device, position and size.
    Webcam,
    /// Cursor highlight, spotlight, click and keystroke display, auto zoom.
    Mouse,
    /// The global hotkey combinations.
    Hotkeys,
    /// Colour and size of the recording control bar.
    ControlBar,
    /// Version, licence, links and the update check.
    About,
}

/// The application: all UI state plus the current recording, if any.
pub struct App {
    /// The user's settings; saved automatically whenever they change.
    settings: Settings,
    /// JSON of the settings as last saved, used to detect changes cheaply.
    saved_json: String,
    /// The registered global hotkeys.
    hotkeys: Hotkeys,
    /// Current step of the recording workflow.
    phase: Phase,
    /// What the main window should currently look like.
    layout: Layout,
    /// The layout, detailed-mode flag and drawing-row flag last applied to the native window;
    /// the window is only reconfigured when one of them changes.
    applied: Option<(Layout, bool, bool)>,
    /// Drawing on the screen while recording (created when first used).
    overlay: Option<crate::overlay::DrawOverlay>,
    /// The desktop icons are hidden by us at the moment.
    icons_hidden: bool,
    /// A newer release found by the update check (offered in the main window).
    update: Option<crate::update::Release>,
    /// Result of an update check still running: (asked for by the user, outcome).
    update_rx: Option<crossbeam_channel::Receiver<(bool, Result<crate::update::Release, String>)>>,
    /// The automatic check already ran (or was not due) in this session.
    update_auto_done: bool,
    /// Outcome of the last check asked for on the About page.
    update_status: Option<(bool, String)>,
    /// When the last marker was dropped (the marker button lights up briefly).
    marker_flash: Option<Instant>,
    /// Set while the control bar asks "Discard this recording?": whether the recording was
    /// already paused before asking (the recording is paused while the question is shown).
    confirm_discard: Option<bool>,
    /// Width in logical points that fits the control bar's buttons, measured while it is shown;
    /// `None` until first measured.
    bar_width: Option<f32>,
    /// Width last given to the control bar window, so it is resized only when the fitted width
    /// changes.
    bar_width_applied: Option<f32>,
    /// Whether the user has dragged the control bar since it was placed; it is then no longer
    /// re-centred when its width changes.
    bar_dragged: bool,
    /// The notification-area icon, present while enabled in the settings.
    tray: Option<crate::tray::Tray>,
    /// Hidden in the tray: the panel stays hidden while nothing is going on.
    in_tray: bool,
    /// Exiting for real (not closing to the tray).
    quitting: bool,
    /// The "start with Windows" value last written to the registry.
    applied_autostart: Option<bool>,
    /// Last known outer position of the panel, restored when switching back to it.
    panel_pos: Option<Pos2>,
    /// The video editor window.
    editor: Editor,
    /// The recordings library window.
    library: crate::library::Library,
    /// The conversion tools window.
    tools: ToolsWindow,
    /// Whether the Options dialog is open.
    options_open: bool,
    /// The selected page of the Options dialog.
    options_tab: OptionsTab,
    /// The scheduled-recording timer.
    timer: TimerState,
    /// Monitors found at start-up (for the full-screen monitor menu and the region border).
    monitors: Vec<MonitorInfo>,
    /// Microphone names, listed lazily on first use and refreshed on demand.
    microphones: Option<Vec<String>>,
    /// Speaker (render device) names, listed lazily.
    speakers: Option<Vec<String>>,
    /// Programs with windows (executable, window title) for "record one program's sound",
    /// and when the list was made.
    programs: Option<(Instant, Vec<(String, String)>)>,
    /// Webcam names, listed lazily.
    webcams: Option<Vec<String>>,
    /// Banner shown on the panel: `(is_error, text)`.
    message: Option<(bool, String)>,
    /// Interrupted recordings found at start-up, offered for recovery one at a time.
    recovery: Vec<FinishedSession>,
    /// The most recently saved recording (for "Play last" and the editor button).
    last_output: Option<PathBuf>,
    #[cfg_attr(not(windows), allow(dead_code))]
    /// Whether the main window is currently hidden from screen capture.
    capture_excluded: Option<bool>,
    /// The theme last applied to the egui context.
    applied_theme: Option<Theme>,
    /// Why the last recording stopped unexpectedly (shown after saving).
    recording_error: Option<String>,
    /// The program icon as a texture, created on first use by the panel header.
    icon_texture: Option<egui::TextureHandle>,
}

impl App {
    /// Creates the application: loads the settings, applies the language and style, registers
    /// the hotkeys and looks for interrupted recordings to recover.
    ///
    /// If the first command-line argument is an existing file, it is opened in the video editor;
    /// with `--tray` (and the tray icon enabled) the window starts hidden.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        crate::i18n::set_language(
            settings
                .language
                .unwrap_or_else(crate::i18n::system_language),
        );
        let mut hotkeys = Hotkeys::new();
        hotkeys.register(&settings.hotkeys());
        let font = draw::load_font(settings.font_bytes());
        setup_style(&cc.egui_ctx);
        let recovery = recorder::find_unfinished_sessions();
        let saved_json = serde_json::to_string(&settings).unwrap_or_default();
        let mut app = Self {
            settings,
            saved_json,
            hotkeys,
            phase: Phase::Idle,
            layout: Layout::Panel,
            applied: None,
            overlay: None,
            marker_flash: None,
            confirm_discard: None,
            bar_width: None,
            bar_width_applied: None,
            bar_dragged: false,
            icons_hidden: false,
            update: None,
            update_rx: None,
            update_auto_done: false,
            update_status: None,
            tray: None,
            in_tray: false,
            quitting: false,
            applied_autostart: None,
            panel_pos: None,
            library: crate::library::Library::new(),
            editor: Editor::new(font),
            tools: ToolsWindow::default(),
            options_open: false,
            options_tab: OptionsTab::General,
            timer: TimerState {
                open: false,
                start: Local::now().format("%H:%M").to_string(),
                end: (Local::now() + chrono::Duration::minutes(30))
                    .format("%H:%M")
                    .to_string(),
                target: TimerTarget::FullScreen,
                sleep_after: false,
                armed: None,
                running: false,
                error: None,
            },
            monitors: geometry::list_monitors(),
            microphones: None,
            speakers: None,
            programs: None,
            webcams: None,
            message: None,
            recovery,
            last_output: None,
            capture_excluded: None,
            applied_theme: None,
            recording_error: None,
            icon_texture: None,
        };
        // Start Media Foundation in the background so the first recording starts instantly.
        std::thread::spawn(|| {
            if let Err(e) = crate::media::mf::init() {
                log::error!("{e:#}");
            }
        });
        // Icons hidden for a recording that ended in a crash: show them again.
        if app.settings.desktop_icons_hidden {
            crate::util::set_desktop_icons_visible(true);
            app.settings.desktop_icons_hidden = false;
        }
        // Started with Windows: stay hidden in the tray until the user opens the window.
        if app.settings.tray_icon && std::env::args().any(|a| a == "--tray") {
            app.in_tray = true;
            app.layout = Layout::Hidden;
        }
        // Open a file passed on the command line in the editor.
        if let Some(arg) = std::env::args().nth(1) {
            let p = PathBuf::from(arg);
            if p.is_file() {
                app.editor.open_file(&p, &cc.egui_ctx);
            }
        }
        app
    }

    /// Returns `true` when no recording is being prepared, made or saved.
    fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    // ------------------------------------------------------------------
    // Starting / stopping

    /// Starts choosing a window or area to record with the selection overlay.
    ///
    /// The panel is hidden first ([`Phase::PendingSelect`]) so that it does not appear in the
    /// screenshot the overlay is drawn on. For audio-only formats there is nothing to select, so
    /// the countdown starts directly.
    fn begin_select(&mut self, mode: SelectMode) {
        if !self.is_idle() {
            return;
        }
        if self.settings.format.is_audio_only() {
            self.begin_countdown(None);
            return;
        }
        self.message = None;
        self.layout = Layout::Hidden;
        self.phase = Phase::PendingSelect {
            mode,
            since: Instant::now(),
        };
    }

    /// Starts recording a whole monitor: `monitor` if given, otherwise the one under the mouse
    /// pointer, falling back to the primary monitor.
    fn begin_full_screen(&mut self, monitor: Option<MonitorInfo>) {
        if !self.is_idle() {
            return;
        }
        let mon = monitor
            .or_else(|| mouse_position().and_then(|(x, y)| geometry::monitor_at(x, y)))
            .or_else(geometry::primary_monitor);
        match mon {
            Some(m) => self.begin_countdown(Some(m.full_region())),
            None => self.message = Some((true, tr("No monitor found.").into())),
        }
    }

    /// Records the last used region again, or asks for a new region if there is none or its
    /// monitor is no longer connected.
    fn begin_repeat_last(&mut self) {
        if !self.is_idle() {
            return;
        }
        match self.settings.last_region {
            Some(r) if geometry::find_monitor(r.monitor_id).is_some() => {
                self.begin_countdown(Some(r))
            }
            _ => self.begin_select(SelectMode::Region),
        }
    }

    /// Shows the countdown before recording `region` (`None` for audio only).
    ///
    /// The region is remembered for "Repeat Last". The countdown window is centred on the
    /// region's monitor (or the primary monitor); a zero-second countdown ends on the next frame.
    fn begin_countdown(&mut self, region: Option<CaptureRegion>) {
        if let Some(r) = region {
            self.settings.last_region = Some(r);
        }
        let mon = region
            .and_then(|r| {
                geometry::list_monitors()
                    .into_iter()
                    .find(|m| m.id == r.monitor_id)
            })
            .or_else(geometry::primary_monitor);
        let size = Vec2::new(200.0, 200.0);
        let center = mon
            .map(|m| m.logical_rect().center())
            .unwrap_or(Pos2::new(400.0, 300.0));
        self.layout = Layout::Countdown(Rect::from_center_size(center, size));
        let secs = self.settings.countdown_seconds as u64;
        self.phase = Phase::Countdown {
            region,
            until: Instant::now() + Duration::from_secs(secs),
        };
    }

    /// Starts the recorder for `region` and switches to the control bar (or hides the window if
    /// the user prefers). Non-fatal problems, such as a microphone that cannot be opened, are
    /// shown as a warning banner; if the recorder cannot start at all, the panel returns with
    /// the error.
    fn start_recording(&mut self, region: Option<CaptureRegion>) {
        match Recorder::start(&self.settings, region) {
            Ok(rec) => {
                if !rec.warnings.is_empty() {
                    self.message = Some((true, rec.warnings.join("\n")));
                }
                self.layout = if self.settings.hide_panel_while_recording {
                    Layout::Hidden
                } else {
                    Layout::Bar
                };
                self.phase = Phase::Recording {
                    rec: Box::new(rec),
                    region,
                };
            }
            Err(e) => {
                self.message = Some((
                    true,
                    tf("Could not start recording: {}", &[&format!("{e:#}")]),
                ));
                self.phase = Phase::Idle;
                self.layout = Layout::Panel;
            }
        }
    }

    /// Stops the current recording and saves it on a background job ([`Phase::Finalizing`]).
    ///
    /// `sleep_after` puts the computer to sleep once the file is written. Does nothing unless a
    /// recording is in progress.
    fn stop_recording(&mut self, sleep_after: bool) {
        self.confirm_discard = None;
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let Phase::Recording { rec, .. } = phase else {
            self.phase = phase;
            return;
        };
        let mut rec = *rec;
        // Collect a capture-thread failure now: the recorder is moved into the job below, and the
        // error explains a short or empty result better than the saving step could.
        let video_error = rec.video_error();
        self.layout = Layout::Panel;
        // Stopping the capture threads and encoding can take a while; keep the UI responsive.
        let job = Job::spawn("Saving recording", move |job| {
            job.set_progress(0.0, tr("Finishing capture…"));
            let session = rec.stop()?;
            let out = compose::finalize(&session, Some(job))?;
            Ok((out, tr("Recording saved").into()))
        });
        if let Some(e) = video_error {
            log::error!("recording stopped: {e}");
            self.recording_error = Some(e);
        }
        self.phase = Phase::Finalizing { job, sleep_after };
    }

    /// Discards the current recording without saving it.
    ///
    /// Saving that has already started cannot be cancelled and continues. Also clears the timer's
    /// running state so that its stop time does not act on a later recording.
    fn cancel_recording(&mut self) {
        self.confirm_discard = None;
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Recording { rec, .. } => {
                // Stopping the threads and deleting the files can block; do it off the UI thread.
                std::thread::spawn(move || rec.cancel());
                self.message = Some((false, tr("Recording discarded.").into()));
            }
            Phase::Finalizing { job, sleep_after } => {
                // Put the saving job back untouched.
                self.phase = Phase::Finalizing { job, sleep_after };
                return;
            }
            _ => {}
        }
        self.layout = Layout::Panel;
        self.timer.running = false;
    }

    /// Performs a hotkey action; also used by the control bar buttons and the tray menu.
    ///
    /// Start/pause starts a recording of the last region (or the full screen) when idle, toggles
    /// pause while recording and skips the rest of a running countdown. Stop and discard act on
    /// a running recording, or abort a selection or countdown that has not started recording.
    fn handle_hotkey(&mut self, action: HotkeyAction) {
        match action {
            HotkeyAction::StartPause => match &mut self.phase {
                Phase::Idle => self.begin_repeat_or_full(),
                Phase::Recording { rec, .. } => rec.toggle_pause(),
                Phase::Countdown { until, .. } => *until = Instant::now(),
                _ => {}
            },
            HotkeyAction::Stop => match &self.phase {
                Phase::Recording { .. } => self.stop_recording(false),
                Phase::Countdown { .. }
                | Phase::Starting { .. }
                | Phase::Selecting(_)
                | Phase::PendingSelect { .. } => self.abort_to_idle(),
                _ => {}
            },
            HotkeyAction::Draw => self.toggle_drawing(),
            HotkeyAction::Marker => {
                if let Phase::Recording { rec, .. } = &mut self.phase {
                    rec.add_marker();
                    self.marker_flash = Some(Instant::now());
                }
            }
            HotkeyAction::Cancel => match &self.phase {
                Phase::Recording { .. } => self.cancel_recording(),
                Phase::Countdown { .. }
                | Phase::Starting { .. }
                | Phase::Selecting(_)
                | Phase::PendingSelect { .. } => self.abort_to_idle(),
                _ => {}
            },
        }
    }

    /// Turns drawing on the screen on or off (only while recording a window or area, not audio).
    ///
    /// The drawing overlay is created on first use, covering the recorded window at its current
    /// position or the recorded area, in physical desktop pixels.
    fn toggle_drawing(&mut self) {
        let Phase::Recording {
            region: Some(r), ..
        } = &self.phase
        else {
            return;
        };
        if self.overlay.is_none() {
            // Place it over the recorded window where it is now, or else over the recorded area
            // (region coordinates are relative to its monitor).
            let rect = match r.window.and_then(crate::media::wgc::window_bounds) {
                Some((x, y, _, _)) => (x, y),
                None => {
                    let Some(m) = geometry::list_monitors()
                        .into_iter()
                        .find(|m| m.id == r.monitor_id)
                    else {
                        return;
                    };
                    (m.x + r.x as i32, m.y + r.y as i32)
                }
            };
            self.overlay = Some(crate::overlay::DrawOverlay::start(
                rect.0,
                rect.1,
                r.width as i32,
                r.height as i32,
            ));
        }
        if let Some(o) = &self.overlay {
            o.set_drawing(!o.is_drawing());
        }
    }

    /// Returns `true` when the control bar should show its second row with the drawing tools:
    /// while drawing is on, or while drawn shapes remain on the screen.
    fn draw_row(&self) -> bool {
        self.overlay
            .as_ref()
            .is_some_and(|o| o.is_drawing() || o.has_shapes())
    }

    /// Returns the programs with windows, for choosing whose sound to record. The list is cached for a few
    /// seconds so menus and the Options page do not query every process on each frame.
    fn program_list(&mut self) -> Vec<(String, String)> {
        let stale = self
            .programs
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(5));
        if stale {
            self.programs = Some((Instant::now(), crate::media::apploop::programs()));
        }
        self.programs
            .as_ref()
            .map(|(_, l)| l.clone())
            .unwrap_or_default()
    }

    /// Starts recording the last region if there is one, otherwise the full screen.
    fn begin_repeat_or_full(&mut self) {
        if self.settings.last_region.is_some() {
            self.begin_repeat_last();
        } else {
            self.begin_full_screen(None);
        }
    }

    /// Abandons a selection or countdown and returns to the capture panel.
    fn abort_to_idle(&mut self) {
        self.phase = Phase::Idle;
        self.layout = Layout::Panel;
    }

    // ------------------------------------------------------------------
    // Per-frame logic (also runs while the main window is hidden)

    /// Applies the theme and the interface language from the settings when they have changed.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme != Some(self.settings.theme) {
            ctx.set_theme(self.settings.theme.preference());
            self.applied_theme = Some(self.settings.theme);
        }
        // No explicit choice means "follow the Windows display language".
        let lang = self
            .settings
            .language
            .unwrap_or_else(crate::i18n::system_language);
        if crate::i18n::language() != lang {
            crate::i18n::set_language(lang);
            ctx.request_repaint();
        }
    }

    /// Runs the per-frame logic that does not depend on the window being visible.
    ///
    /// Handles hotkeys, the tray icon and the timer, keeps the drawing overlay in place, advances
    /// the recording [`Phase`] (selection delay, countdown, auto stop, capture failures, finished
    /// saving), saves changed settings and schedules the next repaint.
    fn tick(&mut self, ctx: &egui::Context) {
        self.apply_theme(ctx);
        self.tick_desktop_icons();
        self.tick_update(ctx);
        self.tick_tray(ctx);
        for action in self.hotkeys.poll() {
            self.handle_hotkey(action);
        }
        self.tick_timer();

        // The drawing overlay only lives as long as the recording; dropping it closes it.
        if !matches!(self.phase, Phase::Recording { .. }) {
            self.overlay = None;
        } else if let Some(o) = &self.overlay {
            // Stay on the recorded window when it moves.
            if let Phase::Recording {
                region:
                    Some(CaptureRegion {
                        window: Some(handle),
                        ..
                    }),
                ..
            } = &self.phase
            {
                if let Some((x, y, _, _)) = crate::media::wgc::window_bounds(*handle) {
                    o.move_to(x, y);
                }
                ctx.request_repaint_after(Duration::from_millis(30));
            }
            // Keep the control bar clickable above the drawing surface: cut a hole for it, in
            // physical pixels (rounded outwards so no edge of the bar is covered).
            let hole = (self.layout == Layout::Bar)
                .then(|| ctx.input(|i| i.viewport().outer_rect))
                .flatten()
                .map(|r| {
                    let ppp = ctx.pixels_per_point();
                    (
                        (r.min.x * ppp).floor() as i32,
                        (r.min.y * ppp).floor() as i32,
                        (r.width() * ppp).ceil() as i32,
                        (r.height() * ppp).ceil() as i32,
                    )
                });
            o.set_hole(hole);
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Advance the workflow. Transitions that replace the phase are collected in `next` and
        // applied after the match, because the phase is borrowed inside it.
        let mut next = None;
        match &mut self.phase {
            Phase::PendingSelect { mode, since } => {
                // Give the window manager time to hide our panel before the overlay takes its
                // screenshot of the monitor under the mouse.
                if since.elapsed() > Duration::from_millis(350) {
                    let mode = *mode;
                    next = Some(match Selector::new(ctx, mode, mouse_position()) {
                        Ok(sel) => {
                            self.layout = Layout::Overlay(sel.logical_rect());
                            Phase::Selecting(Box::new(sel))
                        }
                        Err(e) => {
                            self.message = Some((true, format!("{e:#}")));
                            self.layout = Layout::Panel;
                            Phase::Idle
                        }
                    });
                }
            }
            Phase::Countdown { region, until } => {
                // Hide the countdown, then wait briefly so it has left the screen before the
                // first frame is captured.
                if Instant::now() >= *until {
                    self.layout = Layout::Hidden;
                    next = Some(Phase::Starting {
                        region: *region,
                        at: Instant::now() + Duration::from_millis(300),
                    });
                }
            }
            Phase::Starting { region, at } => {
                if Instant::now() >= *at {
                    let r = *region;
                    self.start_recording(r);
                }
            }
            Phase::Recording { rec, .. } => {
                // Stop automatically after the configured maximum length (0 = unlimited), or
                // save what was recorded if the video capture thread has failed.
                let max = self.settings.max_duration_minutes;
                let over = max > 0 && rec.elapsed() >= Duration::from_secs(max as u64 * 60);
                let failed = rec.video_error();
                if let Some(e) = failed {
                    log::error!("recording stopped: {e}");
                    self.recording_error = Some(e);
                    self.stop_recording(false);
                } else if over {
                    self.stop_recording(false);
                }
            }
            Phase::Finalizing { job, sleep_after } if job.is_finished() => {
                // Saving finished: report the result, then run the "after recording" action.
                let sleep_after = *sleep_after;
                match job.status() {
                    JobStatus::Done { output, .. } => {
                        self.last_output = Some(output.clone());
                        // If capture stopped early, say so and where the partial recording went.
                        match self.recording_error.take() {
                            Some(e) => {
                                self.message = Some((
                                    true,
                                    format!(
                                        "{}\n{}",
                                        tf("Recording stopped early: {}", &[&e]),
                                        tf(
                                            "What was recorded was saved to {}",
                                            &[&output.display()]
                                        )
                                    ),
                                ))
                            }
                            None => {
                                // Do not hide a start-up warning (e.g. a failed microphone)
                                // behind the success message.
                                if self.message.as_ref().is_none_or(|(err, _)| !*err) {
                                    self.message =
                                        Some((false, tf("Saved {}", &[&output.display()])));
                                }
                            }
                        }
                        // A scheduled recording that asked for sleep skips the usual action:
                        // nobody is there to see the editor or folder.
                        if sleep_after {
                            crate::util::sleep_computer();
                        } else {
                            match self.settings.after_recording {
                                AfterRecording::OpenEditor
                                    if !self.settings.format.is_audio_only() =>
                                {
                                    self.editor.open_file(&output, ctx);
                                }
                                // Audio-only files cannot be edited; show them in the folder.
                                AfterRecording::OpenFolder | AfterRecording::OpenEditor => {
                                    crate::util::reveal_in_folder(&output)
                                }
                                AfterRecording::DoNothing => {}
                            }
                        }
                    }
                    JobStatus::Failed(e) => {
                        log::error!("saving failed: {e}");
                        // The capture error explains the failure better than a complaint about
                        // the resulting empty file.
                        self.message = Some((
                            true,
                            match self.recording_error.take() {
                                Some(rec) => tf("Recording failed: {}", &[&rec]),
                                None => tf("Saving failed: {}", &[&e]),
                            },
                        ))
                    }
                    JobStatus::Running { .. } => {}
                }
                next = Some(Phase::Idle);
                self.layout = Layout::Panel;
            }
            _ => {}
        }
        if let Some(p) = next {
            self.phase = p;
        }

        // Persist settings when they change. Wait until no mouse button is held, so dragging a
        // slider does not write the file on every frame.
        if !ctx.input(|i| i.pointer.any_down()) {
            if let Ok(json) = serde_json::to_string(&self.settings) {
                if json != self.saved_json {
                    self.settings.save();
                    self.saved_json = json;
                }
            }
        }

        // Keep ticking even without input: slowly when idle, faster while recording (timer and
        // level meters) and quickly during transitions (selection delay, countdown, saving).
        let interval = match self.phase {
            Phase::Idle => 200,
            Phase::Recording { .. } => 100,
            _ => 30,
        };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }

    /// Keeps the notification-area icon in line with the settings and handles its commands.
    ///
    /// Also applies the "start with Windows" setting, implements close-to-tray, and keeps the
    /// main window hidden while the application lives in the tray.
    fn tick_tray(&mut self, ctx: &egui::Context) {
        // Create or remove the tray icon when the setting changes. Without an icon the user
        // would have no way back to a hidden window, so show it.
        if self.settings.tray_icon != self.tray.is_some() {
            self.tray = self
                .settings
                .tray_icon
                .then(|| crate::tray::Tray::start(ctx));
            if self.tray.is_none() && self.in_tray {
                self.show_window(ctx);
            }
        }
        if self.applied_autostart != Some(self.settings.start_with_windows) {
            crate::tray::set_start_with_windows(self.settings.start_with_windows);
            self.applied_autostart = Some(self.settings.start_with_windows);
        }
        // Mirror the recording state in the tray menu and collect the clicked commands.
        let commands = match &self.tray {
            Some(tray) => {
                let (recording, paused) = match &self.phase {
                    Phase::Recording { rec, .. } => (true, rec.is_paused()),
                    _ => (false, false),
                };
                tray.set_state(recording, paused);
                tray.poll()
            }
            None => vec![],
        };
        for cmd in commands {
            use crate::tray::TrayCommand as T;
            match cmd {
                T::Show => self.show_window(ctx),
                T::Record if self.is_idle() => self.handle_hotkey(HotkeyAction::StartPause),
                T::PauseResume if matches!(self.phase, Phase::Recording { .. }) => {
                    self.handle_hotkey(HotkeyAction::StartPause)
                }
                T::Stop => self.handle_hotkey(HotkeyAction::Stop),
                T::Recordings => self.library.open(),
                T::Options => self.options_open = true,
                T::Exit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                _ => {}
            }
        }

        // Closing the window: keep running in the tray if wanted. Only the panel closes to the
        // tray; closing in any other layout (or via Exit) quits.
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.tray.is_some()
            && self.settings.close_to_tray
            && self.layout == Layout::Panel
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.in_tray = true;
        }

        if self.in_tray {
            // Other windows cannot open while the main one is hidden, so show it when one of
            // them is requested; otherwise keep the idle panel hidden.
            let busy = self.options_open
                || self.timer.open
                || self.tools.open
                || self.editor.open
                || self.library.is_open();
            if busy {
                self.show_window(ctx);
            } else if self.is_idle() && self.layout == Layout::Panel {
                self.layout = Layout::Hidden;
            }
            // Keep answering the tray and hotkeys while hidden.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Starts an update check on a background thread. `manual` checks report their outcome on
    /// the About page; automatic ones only offer a newer version.
    fn start_update_check(&mut self, manual: bool, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = crate::update::latest().map_err(|e| format!("{e:#}"));
            let _ = tx.send((manual, r));
            ctx.request_repaint();
        });
        self.update_rx = Some(rx);
        if manual {
            self.update_status = None;
        }
    }

    /// Runs the automatic update check once a day and handles finished checks.
    fn tick_update(&mut self, ctx: &egui::Context) {
        let now = chrono::Utc::now().timestamp();
        if !self.update_auto_done && self.update_rx.is_none() {
            self.update_auto_done = true;
            if self.settings.check_updates && now - self.settings.last_update_check >= 86_400 {
                self.start_update_check(false, ctx);
            }
        }
        let Some(rx) = &self.update_rx else { return };
        let Ok((manual, result)) = rx.try_recv() else {
            return;
        };
        self.update_rx = None;
        let current = env!("CARGO_PKG_VERSION");
        match result {
            Ok(release) => {
                self.settings.last_update_check = now;
                let newer = crate::update::is_newer(&release.version, current);
                if manual {
                    self.update_status = Some((
                        false,
                        if newer {
                            tf("Version {} is available.", &[&release.version])
                        } else {
                            tf("You have the latest version ({}).", &[&current])
                        },
                    ));
                }
                // A manual check offers even a version that was skipped before.
                let skipped = self.settings.skipped_version.as_deref() == Some(&release.version);
                if newer && (manual || !skipped) {
                    self.update = Some(release);
                }
            }
            Err(e) => {
                log::info!("update check: {e}");
                if manual {
                    self.update_status = Some((true, tf("Could not check for updates: {}", &[&e])));
                }
            }
        }
    }

    /// Hides the desktop icons from the countdown until the recording ends (when enabled), and
    /// shows them again otherwise. The state is saved so a crash cannot leave them hidden.
    fn tick_desktop_icons(&mut self) {
        let want = self.settings.hide_desktop_icons
            && matches!(
                self.phase,
                Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. }
            );
        if want != self.icons_hidden {
            crate::util::set_desktop_icons_visible(!want);
            self.icons_hidden = want;
            self.settings.desktop_icons_hidden = want;
        }
    }

    /// Brings the main window back from the tray (or to the front if it is already shown).
    ///
    /// A hidden window is only turned back into the panel when idle; during a recording the
    /// current layout stays as it is.
    fn show_window(&mut self, ctx: &egui::Context) {
        self.in_tray = false;
        if self.layout == Layout::Hidden && self.is_idle() {
            self.layout = Layout::Panel;
        } else if self.layout == Layout::Panel {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    /// Starts and stops the scheduled recording when the armed times are reached.
    ///
    /// If another recording is in progress at the start time, the scheduled one is skipped and
    /// the timer is disarmed with an explanation.
    fn tick_timer(&mut self) {
        let Some((start, end)) = self.timer.armed else {
            return;
        };
        let now = Local::now();
        if !self.timer.running && now >= start {
            if self.is_idle() {
                self.timer.running = true;
                let target = self.timer.target;
                // Nobody needs a countdown for an unattended recording: start immediately, then
                // restore the user's countdown setting.
                let countdown = self.settings.countdown_seconds;
                self.settings.countdown_seconds = 0;
                match target {
                    TimerTarget::FullScreen => self.begin_full_screen(None),
                    TimerTarget::LastRegion => self.begin_repeat_or_full(),
                }
                self.settings.countdown_seconds = countdown;
            } else {
                self.timer.armed = None;
                self.timer.error =
                    Some(tr("Timer skipped: another recording was in progress.").into());
            }
        }
        // At the stop time, save the recording (it may already have been stopped by hand).
        if self.timer.running && now >= end {
            self.timer.running = false;
            self.timer.armed = None;
            if matches!(self.phase, Phase::Recording { .. }) {
                let sleep = self.timer.sleep_after;
                self.stop_recording(sleep);
            }
        }
    }

    /// Reconfigures the native main window for the current [`Layout`].
    ///
    /// Viewport commands are only sent when the layout, the panel mode or the control bar's
    /// drawing row changed since the last call, so the window is not reset every frame.
    fn apply_layout(&mut self, ctx: &egui::Context) {
        let want = (self.layout, self.settings.detailed_mode, self.draw_row());
        if self.applied == Some(want) {
            return;
        }
        let previous = self.applied.map(|(l, _, _)| l);
        // Remember where the panel was, so it returns to the same place after a recording.
        if matches!(previous, None | Some(Layout::Panel)) {
            if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                self.panel_pos = Some(r.min);
            }
        }
        // Turns the window into an undecorated, always-on-top window covering `rect`.
        let borderless = |ctx: &egui::Context, rect: Rect, focus: bool| {
            ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(10.0, 10.0)));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(rect.size()));
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(rect.min));
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            if focus {
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        };
        match self.layout {
            Layout::Panel => {
                // Restore a normal, decorated window at the panel size and old position. At
                // start-up the window already is one: changing its style while Windows is
                // still drawing it for the first time can leave the title bar unpainted
                // (black, without buttons), so the style is only restored after another layout.
                if previous.is_some() {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
                    ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
                }
                let size = if self.settings.detailed_mode {
                    PANEL_SIZE_DETAILED
                } else {
                    PANEL_SIZE
                };
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(size.into()));
                if !matches!(previous, None | Some(Layout::Panel)) {
                    if let Some(p) = self.panel_pos {
                        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(p));
                    }
                }
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            Layout::Bar => {
                // Place the control bar next to the recorded area without taking focus from
                // the application being recorded, using the width that fits the buttons once it
                // has been measured.
                let mut size = bar_size(&self.settings, self.draw_row());
                if let Some(w) = self.bar_width {
                    size.x = w;
                }
                let pos = bar_position(self.recording_region(), size);
                self.bar_width_applied = Some(size.x);
                self.bar_dragged = false;
                borderless(ctx, Rect::from_min_size(pos, size), false);
            }
            // The overlay takes focus so that it receives Esc and keyboard input.
            Layout::Overlay(rect) => borderless(ctx, rect, true),
            Layout::Countdown(rect) => borderless(ctx, rect, false),
            Layout::Hidden => ctx.send_viewport_cmd(ViewportCommand::Visible(false)),
        }
        self.applied = Some(want);
    }

    // ------------------------------------------------------------------
    // UI

    /// Draws the capture panel: header, capture-mode tiles, device chips, the optional quick
    /// settings card and the action buttons, then fits the window height to the content.
    fn panel_ui(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(14));
        let content = egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    self.panel_header(ui);
                    ui.add_space(4.0);
                    self.capture_tiles(ui);
                    self.device_row(ui);
                    if self.settings.detailed_mode {
                        crate::theme::card(ui, |ui| self.quick_settings(ui));
                    }
                    self.panel_actions(ui);
                })
                .response
                .rect
            })
            .inner;
        // Add the panel's inner margins (2 × 14 points) to the content height.
        self.fit_panel_height(ui.ctx(), content.height() + 28.0);
    }

    /// Sizes the (non-resizable) panel window to `height` logical points of content.
    ///
    /// Only resizes when the height differs by more than 1.5 points, to avoid a feedback loop of
    /// sub-pixel adjustments; does nothing in other layouts.
    fn fit_panel_height(&mut self, ctx: &egui::Context, height: f32) {
        if self.layout != Layout::Panel {
            return;
        }
        let height = height.ceil().max(120.0);
        let current = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height()));
        if current.is_some_and(|h| (h - height).abs() > 1.5) {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(PANEL_WIDTH, height)));
        }
    }

    /// Draws the panel header: program icon and name on the left; detail toggle, About, Options,
    /// language and light/dark switch on the right.
    fn panel_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Upload the program icon as a texture once and reuse it every frame.
            let icon = self.icon_texture.get_or_insert_with(|| {
                let i = crate::util::app_icon();
                ui.ctx().load_texture(
                    "app-icon",
                    egui::ColorImage::from_rgba_unmultiplied(
                        [i.width as usize, i.height as usize],
                        &i.rgba,
                    ),
                    egui::TextureOptions::LINEAR,
                )
            });
            ui.add(egui::Image::new((icon.id(), Vec2::splat(26.0))));
            ui.label(RichText::new(crate::util::APP_NAME).heading().strong());
            // Right-to-left layout: the first button added ends up at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, tip) = if self.settings.detailed_mode {
                    ("⏶", tr("Hide recording settings"))
                } else {
                    ("⏷", tr("Show recording settings"))
                };
                if ui.button(label).on_hover_text(tip).clicked() {
                    self.settings.detailed_mode = !self.settings.detailed_mode;
                }
                if ui.button("ℹ").on_hover_text(tr("About")).clicked() {
                    self.options_tab = OptionsTab::About;
                    self.options_open = true;
                }
                if ui.button(tr("⚙ Options")).clicked() {
                    self.options_open = true;
                }
                ui.menu_button("🌐", |ui| {
                    crate::i18n::language_menu(ui, &mut self.settings.language);
                })
                .response
                .on_hover_text(tr("Language"));
                // One-click switch between light and dark.
                let dark = ui.ctx().theme() == egui::Theme::Dark;
                let (icon, tip) = if dark {
                    ("☀", tr("Switch to light mode"))
                } else {
                    ("🌙", tr("Switch to dark mode"))
                };
                if ui.button(icon).on_hover_text(tip).clicked() {
                    self.settings.theme = if dark { Theme::Light } else { Theme::Dark };
                }
            });
        });
    }

    /// Draws the "What to record" tiles (window, region, fixed region, full screen, repeat last),
    /// or a single "Record audio" tile for audio-only formats. Disabled while not idle.
    fn capture_tiles(&mut self, ui: &mut egui::Ui) {
        let idle = self.is_idle();
        let audio_only = self.settings.format.is_audio_only();
        crate::theme::caption(ui, tr("WHAT TO RECORD"));
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // Five tiles share the row, minus four 10-point gaps.
                let size = Vec2::new(((ui.available_width() - 40.0) / 5.0).floor(), 82.0);
                if audio_only {
                    if crate::theme::tile(ui, "🎤", tr("Record audio"), Vec2::new(size.x * 1.5, size.y))
                        .on_hover_text(tr("Audio-only recording (MP3)"))
                        .clicked()
                    {
                        self.begin_countdown(None);
                    }
                    return;
                }
                if crate::theme::tile(ui, "🗗", tr("Window / Object"), size)
                    .on_hover_text(tr(
                        "Record a window — click it to select. The recording follows the window when you move it, even behind other windows.",
                    ))
                    .clicked()
                {
                    self.begin_select(SelectMode::Window);
                }
                if crate::theme::tile(ui, "⛶", tr("Rectangular Region"), size)
                    .on_hover_text(tr("Drag to select the area to record"))
                    .clicked()
                {
                    self.begin_select(SelectMode::Region);
                }
                let (fw, fh) = (
                    self.settings.fixed_region_width,
                    self.settings.fixed_region_height,
                );
                if crate::theme::tile(ui, "▣", tr("Fixed Region"), size)
                    .on_hover_text(tf("Record a fixed {}×{} area", &[&fw, &fh]))
                    .clicked()
                {
                    self.begin_select(SelectMode::Fixed {
                        width: fw,
                        height: fh,
                    });
                }
                let fs = crate::theme::tile(ui, "🖵", tr("Full Screen"), size).on_hover_text(
                    if self.monitors.len() > 1 {
                        tr("Record the whole screen under the mouse (right-click to choose a monitor)")
                    } else {
                        tr("Record the whole screen")
                    },
                );
                if fs.clicked() {
                    self.begin_full_screen(None);
                }
                // With several monitors, right-clicking Full Screen picks a specific one.
                if self.monitors.len() > 1 {
                    fs.context_menu(|ui| {
                        for m in self.monitors.clone() {
                            if ui
                                .button(format!("{} ({}×{})", m.name, m.width, m.height))
                                .clicked()
                            {
                                self.begin_full_screen(Some(m));
                                ui.close();
                            }
                        }
                    });
                }
                let last = self
                    .settings
                    .last_region
                    .map(|r| r.describe())
                    .unwrap_or_else(|| tr("none yet").into());
                if crate::theme::tile(ui, "↻", tr("Repeat Last"), size)
                    .on_hover_text(tf("Record the last area again ({})", &[&last]))
                    .clicked()
                {
                    self.begin_repeat_last();
                }
            });
        });
    }

    /// Draws the panel's bottom area: buttons for the editor, library, tools, timer, output
    /// folder and last recording, the saving progress bar, the message banner and a hotkey hint.
    fn panel_actions(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            // Open the last MP4 recording directly if it still exists, otherwise ask for a file.
            if ui.button(tr("🎬 Video Editor")).clicked() {
                if let Some(last) = self
                    .last_output
                    .clone()
                    .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "mp4"))
                {
                    self.editor.open_file(&last, ui.ctx());
                } else if let Some(p) = rfd::FileDialog::new()
                    .add_filter(tr("Video"), &["mp4", "mov", "m4v", "avi", "wmv", "mkv"])
                    .pick_file()
                {
                    self.editor.open_file(&p, ui.ctx());
                }
            }
            if ui
                .button(tr("📚 Recordings"))
                .on_hover_text(tr("Browse, play, edit, rename and delete your recordings"))
                .clicked()
            {
                self.library.open();
            }
            ui.menu_button(tr("🛠 Tools"), |ui| {
                if ui.button(tr("Convert to MP4…")).clicked() {
                    self.tools.open_tool(ToolKind::ConvertMp4);
                    ui.close();
                }
                if ui.button(tr("Convert to animated GIF…")).clicked() {
                    self.tools.open_tool(ToolKind::ConvertGif);
                    ui.close();
                }
                if ui.button(tr("Extract audio (M4A / MP3)…")).clicked() {
                    self.tools.open_tool(ToolKind::ExtractAudio);
                    ui.close();
                }
                if ui.button(tr("Join videos…")).clicked() {
                    self.tools.open_tool(ToolKind::Join);
                    ui.close();
                }
            });
            let timer_label = if self.timer.armed.is_some() {
                tr("⏰ Timer (armed)")
            } else {
                tr("⏰ Timer")
            };
            if ui.button(timer_label).clicked() {
                self.timer.open = true;
            }
            if ui.button(tr("📁 Output folder")).clicked() {
                let _ = std::fs::create_dir_all(&self.settings.output_dir);
                crate::util::open_path(&self.settings.output_dir);
            }
            if let Some(last) = self.last_output.clone() {
                if ui
                    .button(tr("▶ Play last"))
                    .on_hover_text(last.display().to_string())
                    .clicked()
                {
                    crate::util::open_path(&last);
                }
            }
        });

        // Progress of saving (or recovering) a recording.
        if let Phase::Finalizing { job, .. } = &self.phase {
            if let JobStatus::Running { progress, message } = job.status() {
                ui.add(
                    egui::ProgressBar::new(progress)
                        .text(message)
                        .show_percentage()
                        .corner_radius(6.0),
                );
            }
        }
        if let Some((is_err, msg)) = self.message.clone() {
            if crate::theme::banner(ui, is_err, &msg) {
                self.message = None;
            }
        }
        // A newer version: download it, skip it, or hide the offer for now.
        if let Some(rel) = self.update.clone() {
            crate::theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("⬆")
                            .color(crate::theme::ACCENT_LIGHT)
                            .size(18.0),
                    );
                    ui.label(
                        RichText::new(tf("Version {} is available.", &[&rel.version])).strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").on_hover_text(tr("Not now")).clicked() {
                            self.update = None;
                        }
                        if ui.button(tr("Skip this version")).clicked() {
                            self.settings.skipped_version = Some(rel.version.clone());
                            self.update = None;
                        }
                        let download =
                            egui::Button::new(RichText::new(tr("Download")).color(Color32::WHITE))
                                .fill(crate::theme::ACCENT);
                        if ui.add(download).clicked() {
                            crate::util::open_path(std::path::Path::new(&rel.url));
                        }
                    });
                });
            });
        }
        let hk = tf(
            "{} start / pause   •   {} stop & save   •   {} discard",
            &[
                &self.settings.hotkey_start_pause,
                &self.settings.hotkey_stop,
                &self.settings.hotkey_cancel,
            ],
        );
        ui.label(RichText::new(format!("⌨  {hk}")).small().weak());
    }

    /// Draws the "Include" row of toggle chips (microphone, speakers, webcam, clicks, cursor
    /// highlight, keystrokes) and the output format selector.
    ///
    /// Right-clicking the audio and webcam chips opens a device menu; device lists are queried
    /// only when a menu is first opened, because enumerating devices is slow. The video-only
    /// chips are hidden for audio-only formats.
    fn device_row(&mut self, ui: &mut egui::Ui) {
        let video = !self.settings.format.is_audio_only();
        crate::theme::caption(ui, tr("INCLUDE"));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            let chip = crate::theme::chip;
            let mic = chip(
                ui,
                &mut self.settings.record_microphone,
                "🎤",
                tr("Microphone"),
            )
            .on_hover_text(tr(
                "Record your voice (right-click to choose the microphone)",
            ));
            mic.context_menu(|ui| {
                let list = self
                    .microphones
                    .get_or_insert_with(audio::list_microphones)
                    .clone();
                device_menu(ui, &list, &mut self.settings.microphone_device);
            });
            let spk = chip(ui, &mut self.settings.record_speakers, "🔊", tr("Speakers"))
                .on_hover_text(tr(
                    "Record the sound playing on this PC (right-click for devices)",
                ));
            spk.context_menu(|ui| {
                let list = self
                    .speakers
                    .get_or_insert_with(audio::list_speakers)
                    .clone();
                device_menu(ui, &list, &mut self.settings.speaker_device);
                ui.separator();
                ui.label(RichText::new(tr("Record sound from:")).weak());
                let programs = self.program_list();
                program_menu(ui, &programs, &mut self.settings.speaker_program);
            });
            if video {
                let cam = chip(ui, &mut self.settings.record_webcam, "📷", tr("Webcam"))
                    .on_hover_text(tr("Show your webcam in a corner (right-click for options)"));
                cam.context_menu(|ui| {
                    if self.webcams.is_none() {
                        self.webcams = Some(crate::media::mf::list_cameras());
                    }
                    let list = self.webcams.clone().unwrap_or_default();
                    device_menu(ui, &list, &mut self.settings.webcam_device);
                    ui.separator();
                    for p in WebcamPosition::ALL {
                        ui.radio_value(&mut self.settings.webcam_position, p, p.label());
                    }
                });
                chip(ui, &mut self.settings.show_clicks, "🖱", tr("Clicks"))
                    .on_hover_text(tr("Show mouse clicks as ripples"));
                chip(
                    ui,
                    &mut self.settings.highlight_cursor,
                    "◎",
                    tr("Highlight"),
                )
                .on_hover_text(tr("Highlight the mouse cursor"));
                chip(ui, &mut self.settings.spotlight, "🔦", tr("Spotlight"))
                    .on_hover_text(tr("Dim everything except the area around the mouse"));
                chip(
                    ui,
                    &mut self.settings.show_keystrokes,
                    "⌨",
                    tr("Keystrokes"),
                )
                .on_hover_text(tr("Show pressed keys in the video"));
            }
        });
        ui.horizontal(|ui| {
            ui.label(tr("Format"));
            egui::ComboBox::from_id_salt("format")
                .width(190.0)
                .selected_text(self.settings.format.label())
                .show_ui(ui, |ui| {
                    for f in OutputFormat::ALL {
                        ui.selectable_value(&mut self.settings.format, f, f.label());
                    }
                });
        });
    }

    /// Draws the "Recording settings" card of the detailed panel: frame rate, quality, output
    /// scale, countdown, volumes, auto stop, the after-recording action and the output folder.
    fn quick_settings(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;
        crate::theme::caption(ui, tr("RECORDING SETTINGS"));
        egui::Grid::new("quick")
            .num_columns(4)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(tr("Frame rate"));
                ui.add(egui::Slider::new(&mut s.fps, 5..=60).suffix(tr(" fps")));
                ui.label(tr("Quality"));
                ui.add(egui::Slider::new(&mut s.quality, 1..=100));
                ui.end_row();
                ui.label(tr("Output size"));
                ui.add(egui::Slider::new(&mut s.scale_percent, 25..=100).suffix(" %"));
                ui.label(tr("Countdown"));
                ui.add(egui::Slider::new(&mut s.countdown_seconds, 0..=10).suffix(" s"));
                ui.end_row();
                ui.label(tr("Mic volume"));
                ui.add(egui::Slider::new(&mut s.microphone_volume, 0.0..=3.0));
                ui.label(tr("Speaker volume"));
                ui.add(egui::Slider::new(&mut s.speaker_volume, 0.0..=3.0));
                ui.end_row();
                ui.label(tr("Auto stop after"));
                ui.add(
                    egui::DragValue::new(&mut s.max_duration_minutes)
                        .range(0..=1440)
                        .suffix(tr(" min (0 = never)")),
                );
                ui.label(tr("After recording"));
                after_recording_combo(ui, &mut s.after_recording);
                ui.end_row();
            });
        ui.horizontal(|ui| {
            ui.label(tr("Save to"));
            ui.add(
                egui::Label::new(RichText::new(s.output_dir.display().to_string()).monospace())
                    .truncate(),
            );
            if ui.button(tr("Change…")).clicked() {
                if let Some(d) = rfd::FileDialog::new()
                    .set_directory(&s.output_dir)
                    .pick_folder()
                {
                    s.output_dir = d;
                }
            }
        });
    }

    /// Draws the recording control bar: blinking record dot, elapsed time, audio level meters,
    /// pause/resume, save, discard and (for window or area recordings) the drawing button, with
    /// an optional second row of drawing tools.
    ///
    /// Colours and size follow the control bar settings. Button clicks are collected and handled
    /// after drawing, because the recorder is borrowed while the bar is drawn. Discarding asks
    /// for confirmation in a modal dialog first.
    fn control_bar(&mut self, ui: &mut egui::Ui) {
        let scale = self.settings.control_bar_scale.clamp(0.7, 2.0);
        let bg = self.settings.control_bar_color;
        let bg = Color32::from_rgb(bg[0], bg[1], bg[2]);
        // Black text on light backgrounds, white on dark ones (simple sum-of-channels test).
        let fg = if (bg.r() as u32 + bg.g() as u32 + bg.b() as u32) > 380 {
            Color32::BLACK
        } else {
            Color32::WHITE
        };
        let mut action = None;
        // Answer to "Discard this recording?": `Some(true)` discards, `Some(false)` keeps recording.
        let mut discard = None;
        let can_draw = matches!(
            self.phase,
            Phase::Recording {
                region: Some(_),
                ..
            }
        );
        let draw_row = self.draw_row();
        // Width of the bar's content, to fit the window to it.
        let content_width = egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(BAR_MARGIN_X as i8, 4)),
            )
            .show(ui, |ui| {
                // The borderless bar has no title bar: dragging its background moves the window.
                let resp = ui.interact(
                    ui.max_rect(),
                    egui::Id::unique("bar-drag"),
                    egui::Sense::drag(),
                );
                if resp.drag_started() {
                    self.bar_dragged = true;
                    ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                }
                let Phase::Recording { rec, .. } = &mut self.phase else {
                    return None;
                };
                let btn = |ui: &mut egui::Ui, t: &str| {
                    ui.add(egui::Button::new(RichText::new(t).size(14.0 * scale)))
                };
                if self.confirm_discard.is_some() {
                    // Ask inside the bar itself: a dialog would not fit in the small bar window.
                    let row = ui.horizontal(|ui| {
                        ui.set_min_height(32.0 * scale);
                        ui.spacing_mut().item_spacing.x = 8.0 * scale;
                        ui.label(
                            RichText::new(tr("Discard this recording?"))
                                .size(14.0 * scale)
                                .strong()
                                .color(fg),
                        );
                        if btn(ui, tr("Discard")).clicked() {
                            discard = Some(true);
                        }
                        if btn(ui, tr("Keep recording")).clicked() {
                            discard = Some(false);
                        }
                    });
                    return Some(row.response.rect.width());
                }
                let rows = ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(32.0 * scale);
                        ui.spacing_mut().item_spacing.x = 8.0 * scale;
                        // Record dot blinking twice a second, or a pause sign while paused.
                        let paused = rec.is_paused();
                        let blink = (ui.input(|i| i.time) * 2.0) as i64 % 2 == 0;
                        let dot = if paused {
                            "⏸"
                        } else if blink {
                            "⏺"
                        } else {
                            "○"
                        };
                        ui.label(
                            RichText::new(dot)
                                .color(crate::theme::RECORD_RED)
                                .size(18.0 * scale),
                        );
                        ui.label(
                            RichText::new(format_time(rec.elapsed().as_secs_f64()))
                                .monospace()
                                .size(16.0 * scale)
                                .color(fg),
                        );
                        if rec.has_mic() {
                            level_meter(ui, "🎤", rec.mic_level.take(), fg, scale);
                        }
                        if rec.has_speakers() {
                            level_meter(ui, "🔊", rec.speaker_level.take(), fg, scale);
                        }
                        if btn(
                            ui,
                            if paused {
                                tr("▶ Resume")
                            } else {
                                tr("⏸ Pause")
                            },
                        )
                        .clicked()
                        {
                            action = Some(HotkeyAction::StartPause);
                        }
                        if btn(ui, tr("⏹ Save")).clicked() {
                            action = Some(HotkeyAction::Stop);
                        }
                        if btn(ui, "✖")
                            .on_hover_text(tr("Discard recording"))
                            .clicked()
                        {
                            action = Some(HotkeyAction::Cancel);
                        }
                        // Marker button: shows how many markers were dropped and lights up
                        // briefly after each one.
                        let n = rec.marker_count();
                        let flash = self
                            .marker_flash
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
                        let label = if n > 0 {
                            format!("🚩 {n}")
                        } else {
                            "🚩".into()
                        };
                        let b = egui::Button::new(RichText::new(label).size(14.0 * scale))
                            .selected(flash);
                        if ui
                            .add(b)
                            .on_hover_text(tf(
                                "Drop a marker ({}); markers show on the editor timeline",
                                &[&self.settings.hotkey_marker],
                            ))
                            .clicked()
                        {
                            action = Some(HotkeyAction::Marker);
                        }
                        if can_draw {
                            let on = self.overlay.as_ref().is_some_and(|o| o.is_drawing());
                            let b =
                                egui::Button::new(RichText::new(tr("✏ Draw")).size(14.0 * scale))
                                    .selected(on);
                            if ui
                                .add(b)
                                .on_hover_text(tf(
                                    "Draw on the screen ({}); right-click or Esc to stop",
                                    &[&self.settings.hotkey_draw],
                                ))
                                .clicked()
                            {
                                action = Some(HotkeyAction::Draw);
                            }
                        }
                    });
                    if draw_row {
                        if let Some(o) = &self.overlay {
                            draw_tools(ui, o, scale);
                        }
                    }
                });
                Some(rows.response.rect.width())
            })
            .inner;
        if let Some(w) = content_width {
            self.fit_bar_width(ui.ctx(), w + 2.0 * BAR_MARGIN_X);
        }
        match discard {
            Some(true) => {
                self.confirm_discard = None;
                self.cancel_recording();
            }
            Some(false) => {
                if let (Some(was_paused), Phase::Recording { rec, .. }) =
                    (self.confirm_discard.take(), &mut self.phase)
                {
                    if !was_paused {
                        rec.resume();
                    }
                }
            }
            None => {}
        }
        if let Some(a) = action {
            if a == HotkeyAction::Cancel {
                // Pause while asking, so the question does not keep recording.
                if let Phase::Recording { rec, .. } = &mut self.phase {
                    self.confirm_discard = Some(rec.is_paused());
                    rec.pause();
                }
            } else {
                self.handle_hotkey(a);
            }
        }
    }

    /// The recorded area of the running recording; `None` when not recording or audio only.
    fn recording_region(&self) -> Option<CaptureRegion> {
        match &self.phase {
            Phase::Recording { region, .. } => *region,
            _ => None,
        }
    }

    /// Narrows or widens the control bar window to `width` logical points so it has no empty
    /// space beside the buttons, and places it again so it stays centred (unless the user has
    /// dragged it, in which case only the size changes).
    ///
    /// Acts only when the width differs by more than 1.5 points from the width last applied, so
    /// it sends no window commands on ordinary frames.
    fn fit_bar_width(&mut self, ctx: &egui::Context, width: f32) {
        if self.layout != Layout::Bar {
            return;
        }
        let width = width.ceil();
        self.bar_width = Some(width);
        if self
            .bar_width_applied
            .is_some_and(|w| (w - width).abs() <= 1.5)
        {
            return;
        }
        self.bar_width_applied = Some(width);
        let size = Vec2::new(width, bar_size(&self.settings, self.draw_row()).y);
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        if !self.bar_dragged {
            let pos = bar_position(self.recording_region(), size);
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
        }
    }

    /// Draws a border around the recorded area (red, amber while paused) when enabled.
    ///
    /// The border consists of four thin, always-on-top, click-through viewports placed just
    /// outside the area so that they are not captured. No border is drawn for full-screen and
    /// window recordings.
    fn region_border(&self, ctx: &egui::Context) {
        if !self.settings.show_region_border {
            return;
        }
        let Phase::Recording {
            region: Some(r), ..
        } = &self.phase
        else {
            return;
        };
        if r.window.is_some() {
            return; // the window moves; a fixed border would be wrong
        }
        let Some(mon) = self
            .monitors
            .iter()
            .find(|m| m.id == r.monitor_id)
            .cloned()
            .or_else(|| {
                geometry::list_monitors()
                    .into_iter()
                    .find(|m| m.id == r.monitor_id)
            })
        else {
            return;
        };
        if r.width == mon.width && r.height == mon.height {
            return; // full screen: no border
        }
        // Convert the region from physical pixels relative to its monitor into logical desktop
        // points, the unit egui uses to place viewports.
        let upp = mon.units_per_point();
        let base = mon.logical_rect().min;
        let rect = Rect::from_min_size(
            base + Vec2::new(r.x as f32 / upp, r.y as f32 / upp),
            Vec2::new(r.width as f32 / upp, r.height as f32 / upp),
        );
        // Border thickness in points, and the top, bottom, left and right strips. The top and
        // bottom strips extend over the corners.
        let t = 3.0;
        let pieces = [
            Rect::from_min_size(
                rect.min - Vec2::splat(t),
                Vec2::new(rect.width() + 2.0 * t, t),
            ),
            Rect::from_min_size(
                Pos2::new(rect.left() - t, rect.bottom()),
                Vec2::new(rect.width() + 2.0 * t, t),
            ),
            Rect::from_min_size(
                Pos2::new(rect.left() - t, rect.top()),
                Vec2::new(t, rect.height()),
            ),
            Rect::from_min_size(
                Pos2::new(rect.right(), rect.top()),
                Vec2::new(t, rect.height()),
            ),
        ];
        let paused = matches!(&self.phase, Phase::Recording { rec, .. } if rec.is_paused());
        let color = if paused {
            Color32::from_rgb(255, 193, 7)
        } else {
            Color32::from_rgb(229, 57, 53)
        };
        for (i, p) in pieces.iter().enumerate() {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of(("border", i)),
                egui::ViewportBuilder::default()
                    .with_title(tr("Softromeda Screen Recorder border"))
                    .with_decorations(false)
                    .with_position(p.min)
                    .with_inner_size(p.size())
                    .with_always_on_top()
                    .with_taskbar(false)
                    .with_resizable(false)
                    .with_mouse_passthrough(true),
                |ui, _| {
                    ui.painter()
                        .rect_filled(ui.ctx().content_rect(), 0.0, color);
                },
            );
        }
    }

    /// Draws the countdown window: the remaining whole seconds and a "Recording starts…" note.
    fn countdown_ui(&self, ui: &mut egui::Ui) {
        let Phase::Countdown { until, .. } = &self.phase else {
            return;
        };
        let remaining = until
            .saturating_duration_since(Instant::now())
            .as_secs_f32();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_gray(20)))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().text(
                    r.center() - Vec2::new(0.0, 12.0),
                    Align2::CENTER_CENTER,
                    format!("{}", remaining.ceil() as u32),
                    FontId::proportional(96.0),
                    Color32::WHITE,
                );
                ui.painter().text(
                    r.center() + Vec2::new(0.0, 66.0),
                    Align2::CENTER_CENTER,
                    tr("Recording starts…"),
                    FontId::proportional(14.0),
                    Color32::LIGHT_GRAY,
                );
            });
    }

    /// Shows the Options dialog in its own viewport while it is open.
    ///
    /// The dialog has a sidebar of pages, a bottom bar with "Restore defaults" and "Close", and
    /// the selected page in the middle. Settings take effect as they are edited, except the
    /// hotkeys and the video editor's text font, which are applied when the dialog closes.
    fn options_ui(&mut self, ctx: &egui::Context) {
        if !self.options_open {
            return;
        }
        let mut close = false;
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of("options"),
            geometry::centered(
                egui::ViewportBuilder::default()
                    .with_title(tr("Options"))
                    .with_inner_size([780.0, 560.0])
                    .with_min_inner_size([640.0, 420.0])
                    .with_max_inner_size([1000.0, 900.0])
                    .with_maximize_button(false)
                    .with_visible(false),
                [780.0, 560.0],
            ),
            |ui, _| {
                // The viewport starts invisible and is shown once its layout has settled.
                crate::util::reveal_when_ready(ui.ctx());
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                // Sidebar navigation; the selected page is highlighted in the accent colour.
                egui::Panel::left("options-nav")
                    .resizable(false)
                    .exact_size(190.0)
                    .frame(
                        egui::Frame::side_top_panel(ui.style())
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                            for (tab, icon, name) in [
                                (OptionsTab::General, "⚙", tr("General")),
                                (OptionsTab::Video, "🎬", tr("Video")),
                                (OptionsTab::Audio, "🔊", tr("Audio")),
                                (OptionsTab::Webcam, "📷", tr("Webcam")),
                                (OptionsTab::Mouse, "🖱", tr("Mouse & Keyboard")),
                                (OptionsTab::Hotkeys, "⌨", tr("Hotkeys")),
                                (OptionsTab::ControlBar, "⏺", tr("Control bar")),
                                (OptionsTab::About, "ℹ", tr("About")),
                            ] {
                                let selected = self.options_tab == tab;
                                let text = RichText::new(format!("{icon}   {name}"));
                                let text = if selected {
                                    text.color(Color32::WHITE)
                                } else {
                                    text
                                };
                                if ui
                                    .add(
                                        egui::Button::new(text)
                                            .fill(if selected {
                                                crate::theme::ACCENT
                                            } else {
                                                Color32::TRANSPARENT
                                            })
                                            .frame_when_inactive(selected)
                                            .min_size(Vec2::new(0.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    self.options_tab = tab;
                                }
                            }
                        });
                    });
                egui::Panel::bottom("options-buttons")
                    .frame(
                        egui::Frame::side_top_panel(ui.style())
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // Reset everything except the remembered region for "Repeat Last".
                            if ui.button(tr("Restore defaults")).clicked() {
                                let last = self.settings.last_region;
                                self.settings = Settings {
                                    last_region: last,
                                    ..Settings::default()
                                };
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(tr("Close")).color(Color32::WHITE),
                                            )
                                            .fill(crate::theme::ACCENT),
                                        )
                                        .clicked()
                                    {
                                        close = true;
                                    }
                                },
                            );
                        });
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(16)),
                    )
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| self.options_tab_ui(ui));
                    });
            },
        );
        // Apply the settings that are expensive to change on every keystroke.
        if close {
            self.options_open = false;
            crate::util::forget_reveal(ctx, ViewportId::from_hash_of("options"));
            self.hotkeys.register(&self.settings.hotkeys());
            self.editor
                .set_font(draw::load_font(self.settings.font_bytes()));
        }
    }

    /// Draws the contents of the selected Options page.
    fn options_tab_ui(&mut self, ui: &mut egui::Ui) {
        match self.options_tab {
            OptionsTab::General => {
                let s = &mut self.settings;
                ui.heading(tr("Output"));
                ui.horizontal(|ui| {
                    ui.label(tr("Folder:"));
                    ui.monospace(s.output_dir.display().to_string());
                    if ui.button(tr("Change…")).clicked() {
                        if let Some(d) = rfd::FileDialog::new().pick_folder() {
                            s.output_dir = d;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(tr("File name template:"));
                    ui.text_edit_singleline(&mut s.filename_template);
                });
                ui.label(
                    RichText::new(tf("Example: {}", &[&s.next_output_path().display()]))
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    ui.label(tr("After recording:"));
                    after_recording_combo(ui, &mut s.after_recording);
                });
                ui.add_space(8.0);
                ui.heading(tr("Troubleshooting"));
                ui.horizontal(|ui| {
                    if ui.button(tr("Open log file")).clicked() {
                        crate::util::open_path(&crate::util::log_path());
                    }
                    if ui.button(tr("Show in folder")).clicked() {
                        crate::util::reveal_in_folder(&crate::util::log_path());
                    }
                });
                ui.label(
                    RichText::new(crate::util::log_path().display().to_string())
                        .small()
                        .weak(),
                );
                ui.add_space(8.0);
                ui.heading(tr("Appearance"));
                ui.horizontal(|ui| {
                    ui.label(tr("Theme:"));
                    for t in Theme::ALL {
                        ui.radio_value(&mut s.theme, t, t.label());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(tr("Language:"));
                    crate::i18n::language_combo(ui, &mut s.language);
                });
                ui.add_space(8.0);
                ui.heading(tr("Notification area"));
                ui.checkbox(
                    &mut s.tray_icon,
                    tr("Show an icon in the notification area"),
                );
                ui.add_enabled_ui(s.tray_icon, |ui| {
                    ui.checkbox(
                        &mut s.close_to_tray,
                        tr("Closing the window keeps Softromeda Screen Recorder running there"),
                    );
                    ui.checkbox(
                        &mut s.start_with_windows,
                        tr("Start with Windows (in the notification area)"),
                    );
                });
                ui.heading(tr("Text font"));
                let s = &mut self.settings;
                ui.label(tr("Optional TTF/OTF font for keystrokes, captions and title pages (e.g. for Korean or other scripts)."));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut s.custom_font_path);
                    if ui.button(tr("Browse…")).clicked() {
                        if let Some(f) = rfd::FileDialog::new()
                            .add_filter("Font", &["ttf", "otf", "ttc"])
                            .pick_file()
                        {
                            s.custom_font_path = f.display().to_string();
                        }
                    }
                });
            }
            OptionsTab::Video => {
                let s = &mut self.settings;
                egui::Grid::new("video-grid")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(tr("Format"));
                        egui::ComboBox::from_id_salt("fmt2")
                            .selected_text(s.format.label())
                            .show_ui(ui, |ui| {
                                for f in OutputFormat::ALL {
                                    ui.selectable_value(&mut s.format, f, f.label());
                                }
                            });
                        ui.end_row();
                        ui.label(tr("Frame rate"));
                        ui.add(egui::Slider::new(&mut s.fps, 5..=60).suffix(tr(" fps")));
                        ui.end_row();
                        ui.label(tr("Quality"));
                        ui.add(egui::Slider::new(&mut s.quality, 1..=100));
                        ui.end_row();
                        ui.label(tr("Output size"));
                        ui.add(egui::Slider::new(&mut s.scale_percent, 25..=100).suffix(" %"));
                        ui.end_row();
                        ui.label(tr("Countdown"));
                        ui.add(egui::Slider::new(&mut s.countdown_seconds, 0..=10).suffix(" s"));
                        ui.end_row();
                        ui.label(tr("Auto stop after"));
                        ui.add(
                            egui::DragValue::new(&mut s.max_duration_minutes)
                                .range(0..=1440)
                                .suffix(tr(" min (0 = never)")),
                        );
                        ui.end_row();
                        ui.label(tr("Fixed region size"));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut s.fixed_region_width).range(16..=7680),
                            );
                            ui.label("×");
                            ui.add(
                                egui::DragValue::new(&mut s.fixed_region_height).range(16..=4320),
                            );
                            for (w, h) in [(640, 480), (1280, 720), (1920, 1080)] {
                                if ui.small_button(format!("{w}×{h}")).clicked() {
                                    s.fixed_region_width = w;
                                    s.fixed_region_height = h;
                                }
                            }
                        });
                        ui.end_row();
                    });
                ui.checkbox(
                    &mut s.show_region_border,
                    tr("Show a border around the recording area"),
                );
                ui.checkbox(
                    &mut s.hide_desktop_icons,
                    tr("Hide desktop icons while recording"),
                )
                .on_hover_text(tr("They come back when the recording ends."));
                ui.checkbox(
                    &mut s.hide_panel_while_recording,
                    tr("Hide the control bar while recording (use hotkeys)"),
                );
                if cfg!(windows) {
                    ui.label(
                        RichText::new(tr(
                            "On Windows the control bar is never captured in the video.",
                        ))
                        .small()
                        .weak(),
                    );
                }
            }
            OptionsTab::Audio => {
                // Forget the cached device lists so they are enumerated again below.
                if ui.button(tr("⟳ Refresh devices")).clicked() {
                    self.microphones = None;
                    self.speakers = None;
                    self.programs = None;
                }
                let mics = self
                    .microphones
                    .get_or_insert_with(audio::list_microphones)
                    .clone();
                let spks = self
                    .speakers
                    .get_or_insert_with(audio::list_speakers)
                    .clone();
                let programs = self.program_list();
                let s = &mut self.settings;
                ui.checkbox(&mut s.record_microphone, tr("Record microphone"));
                device_combo(ui, "mic-dev", &mics, &mut s.microphone_device);
                ui.add(
                    egui::Slider::new(&mut s.microphone_volume, 0.0..=3.0)
                        .text(tr("Microphone volume")),
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.mic_noise_reduction, tr("Reduce background noise"))
                        .on_hover_text(tr(
                            "Removes steady noise such as fans, hum and hiss from the microphone when the recording is saved.",
                        ));
                    ui.add_enabled_ui(s.mic_noise_reduction, |ui| {
                        egui::ComboBox::from_id_salt("noise-strength")
                            .selected_text(s.noise_strength.label())
                            .show_ui(ui, |ui| {
                                for n in crate::settings::NoiseStrength::ALL {
                                    ui.selectable_value(&mut s.noise_strength, n, n.label());
                                }
                            });
                    });
                });
                ui.checkbox(&mut s.mic_leveling, tr("Even out the voice volume"))
                    .on_hover_text(tr(
                        "Raises quiet speech, lowers loud speech and prevents clipping when the recording is saved.",
                    ));
                ui.add_space(8.0);
                ui.checkbox(&mut s.record_speakers, tr("Record speakers (system audio)"));
                device_combo(ui, "spk-dev", &spks, &mut s.speaker_device);
                ui.horizontal(|ui| {
                    ui.label(tr("Record sound from:"));
                    let current = s
                        .speaker_program
                        .clone()
                        .map(|e| e.trim_end_matches(".exe").to_string())
                        .unwrap_or_else(|| tr("All programs").to_string());
                    egui::ComboBox::from_id_salt("spk-program")
                        .width(300.0)
                        .selected_text(current)
                        .show_ui(ui, |ui| program_menu(ui, &programs, &mut s.speaker_program));
                })
                .response
                .on_hover_text(tr(
                    "Record only one program's sound (Windows 10 version 2004 or newer), so notification sounds and other programs stay out of the recording.",
                ));
                ui.add(
                    egui::Slider::new(&mut s.speaker_volume, 0.0..=3.0).text(tr("Speaker volume")),
                );
                ui.checkbox(
                    &mut s.auto_boost_speakers,
                    tr("Auto-boost volume when recording speakers"),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(tr(
                        "Speakers are recorded with WASAPI loopback (whatever you hear).",
                    ))
                    .small()
                    .weak(),
                );
            }
            OptionsTab::Webcam => {
                if ui.button(tr("⟳ Refresh cameras")).clicked() || self.webcams.is_none() {
                    self.webcams = Some(crate::media::mf::list_cameras());
                }
                let cams = self.webcams.clone().unwrap_or_default();
                let s = &mut self.settings;
                ui.checkbox(
                    &mut s.record_webcam,
                    tr("Include webcam (picture-in-picture)"),
                );
                device_combo(ui, "cam-dev", &cams, &mut s.webcam_device);
                ui.horizontal(|ui| {
                    ui.label(tr("Position:"));
                    for p in WebcamPosition::ALL {
                        ui.radio_value(&mut s.webcam_position, p, p.label());
                    }
                });
                ui.add(
                    egui::Slider::new(&mut s.webcam_size_percent, 8..=50)
                        .text(tr("Size (% of video width)")),
                );
            }
            OptionsTab::Mouse => {
                let s = &mut self.settings;
                ui.checkbox(&mut s.show_cursor, tr("Show mouse cursor"));
                ui.checkbox(&mut s.highlight_cursor, tr("Highlight mouse cursor"));
                ui.horizontal(|ui| {
                    ui.label(tr("Highlight colour:"));
                    ui.color_edit_button_srgba_unmultiplied(&mut s.highlight_color);
                    ui.add(
                        egui::Slider::new(&mut s.highlight_radius, 10.0..=80.0).text(tr("Radius")),
                    );
                });
                ui.checkbox(&mut s.smooth_cursor, tr("Smooth mouse movement"))
                    .on_hover_text(tr(
                        "The pointer glides instead of shaking. Clicks are still shown where they happened.",
                    ));
                ui.checkbox(&mut s.spotlight, tr("Spotlight around the mouse"));
                ui.add_enabled_ui(s.spotlight, |ui| {
                    ui.add(
                        egui::Slider::new(&mut s.spotlight_radius, 60.0..=400.0)
                            .text(tr("Spotlight size")),
                    );
                    ui.add(
                        egui::Slider::new(&mut s.spotlight_dim, 0.2..=0.9)
                            .text(tr("Darken the rest")),
                    );
                });
                ui.checkbox(&mut s.show_clicks, tr("Show mouse clicks"));
                ui.horizontal(|ui| {
                    ui.label(tr("Left click:"));
                    ui.color_edit_button_srgba_unmultiplied(&mut s.left_click_color);
                    ui.label(tr("Right click:"));
                    ui.color_edit_button_srgba_unmultiplied(&mut s.right_click_color);
                });
                ui.checkbox(&mut s.click_sound, tr("Play a click sound in the video"))
                    .on_hover_text(tr(
                        "Adds a short click sound to the audio at each left click in the recorded area, when the recording is saved.",
                    ));
                ui.checkbox(&mut s.show_keystrokes, tr("Show keystrokes in the video"));
                ui.add_space(6.0);
                ui.checkbox(&mut s.auto_zoom, tr("Zoom in on clicks automatically"))
                    .on_hover_text(tr(
                        "When a new recording opens in the video editor, it zooms in around your mouse clicks. Adjust or delete the zooms before exporting.",
                    ));
            }
            OptionsTab::Hotkeys => {
                let s = &mut self.settings;
                ui.label(tr("Examples: Ctrl+F11, Ctrl+Shift+R, Alt+F9, Cmd+Shift+5. Applied when Options is closed."));
                egui::Grid::new("hk")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(tr("Start / Pause / Resume"));
                        ui.text_edit_singleline(&mut s.hotkey_start_pause);
                        ui.end_row();
                        ui.label(tr("Stop and save"));
                        ui.text_edit_singleline(&mut s.hotkey_stop);
                        ui.end_row();
                        ui.label(tr("Discard"));
                        ui.text_edit_singleline(&mut s.hotkey_cancel);
                        ui.end_row();
                        ui.label(tr("Draw on screen"));
                        ui.text_edit_singleline(&mut s.hotkey_draw);
                        ui.end_row();
                        ui.label(tr("Drop a marker"));
                        ui.text_edit_singleline(&mut s.hotkey_marker);
                        ui.end_row();
                    });
                for e in &self.hotkeys.errors {
                    ui.colored_label(crate::util::error_color(ui), e);
                }
            }
            OptionsTab::ControlBar => {
                let s = &mut self.settings;
                ui.horizontal(|ui| {
                    ui.label(tr("Colour:"));
                    ui.color_edit_button_srgba_unmultiplied(&mut s.control_bar_color);
                });
                ui.add(egui::Slider::new(&mut s.control_bar_scale, 0.8..=1.8).text(tr("Size")));
            }
            OptionsTab::About => self.about_ui(ui),
        }
    }

    /// Draws the About page: icon, name, version, developer, website and licence.
    fn about_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if let Some(icon) = &self.icon_texture {
                ui.add(egui::Image::new((icon.id(), Vec2::splat(64.0))));
            }
            ui.vertical(|ui| {
                ui.label(RichText::new(crate::util::APP_NAME).size(22.0).strong());
                ui.label(tf("Version {}", &[&env!("CARGO_PKG_VERSION")]));
                ui.label(RichText::new(tr("Screen recorder and video editor for Windows.")).weak());
            });
        });
        ui.add_space(12.0);
        crate::theme::card(ui, |ui| {
            egui::Grid::new("about-grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label(RichText::new(tr("Developer:")).weak());
                    ui.label("Softromeda");
                    ui.end_row();
                    ui.label(RichText::new(tr("Website:")).weak());
                    ui.hyperlink_to(
                        "softromeda.com/screen-recorder",
                        "https://softromeda.com/screen-recorder",
                    );
                    ui.end_row();
                    ui.label(RichText::new(tr("License:")).weak());
                    ui.hyperlink_to(
                        "GNU GPL v3.0 or later",
                        "https://www.gnu.org/licenses/gpl-3.0.html",
                    );
                    ui.end_row();
                    ui.label(RichText::new(tr("Source code:")).weak());
                    ui.hyperlink_to(
                        "github.com/softromeda/screenrecorder",
                        "https://github.com/softromeda/screenrecorder",
                    );
                    ui.end_row();
                });
        });
        // Appropriate Legal Notices (GPL section 0) and the attribution that the additional
        // terms in NOTICE require every version, including modified ones, to keep showing.
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "© 2026 Softromeda · Originally created by Softromeda - https://softromeda.com/screen-recorder",
            )
            .small()
            .weak(),
        );
        ui.label(
            RichText::new(tr(
                "This program comes with ABSOLUTELY NO WARRANTY. It is free software: you may redistribute and change it under the terms of the GNU General Public License.",
            ))
            .small()
            .weak(),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let checking = self.update_rx.is_some();
            if ui
                .add_enabled(!checking, egui::Button::new(tr("Check for updates")))
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_update_check(true, &ctx);
            }
            if checking {
                ui.spinner();
            } else if let Some((err, text)) = &self.update_status {
                let color = if *err {
                    crate::util::error_color(ui)
                } else {
                    crate::util::ok_color(ui)
                };
                ui.colored_label(color, text.as_str());
            }
        });
        ui.checkbox(
            &mut self.settings.check_updates,
            tr("Check for updates automatically"),
        );
    }

    /// Shows the Scheduled Recording (timer) dialog in its own viewport while it is open.
    ///
    /// The start and stop times and options can only be edited while the timer is disarmed.
    /// Arming validates the times with [`schedule`]; while armed, the dialog shows the schedule
    /// and the time left until the start.
    fn timer_ui(&mut self, ctx: &egui::Context) {
        if !self.timer.open {
            return;
        }
        let mut close = false;
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of("timer"),
            geometry::centered(
                egui::ViewportBuilder::default()
                    .with_title(tr("Scheduled Recording"))
                    .with_inner_size([420.0, 300.0])
                    .with_min_inner_size([380.0, 260.0])
                    .with_max_inner_size([640.0, 480.0])
                    .with_maximize_button(false)
                    .with_visible(false),
                [420.0, 300.0],
            ),
            |ui, _| {
                crate::util::reveal_when_ready(ui.ctx());
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    let t = &mut self.timer;
                    ui.heading(tr("Timer"));
                    ui.add_enabled_ui(t.armed.is_none(), |ui| {
                        egui::Grid::new("timer-grid")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.label(tr("Start at (HH:MM)"));
                                ui.text_edit_singleline(&mut t.start);
                                ui.end_row();
                                ui.label(tr("Stop at (HH:MM)"));
                                ui.text_edit_singleline(&mut t.end);
                                ui.end_row();
                                ui.label(tr("Record"));
                                ui.horizontal(|ui| {
                                    ui.radio_value(
                                        &mut t.target,
                                        TimerTarget::FullScreen,
                                        tr("Full screen"),
                                    );
                                    ui.radio_value(
                                        &mut t.target,
                                        TimerTarget::LastRegion,
                                        tr("Last region"),
                                    );
                                });
                                ui.end_row();
                            });
                        ui.checkbox(
                            &mut t.sleep_after,
                            tr("Put the computer to sleep when the recording is complete"),
                        );
                    });
                    ui.add_space(6.0);
                    match t.armed {
                        None => {
                            if ui.button(tr("⏰ Arm timer")).clicked() {
                                match schedule(&t.start, &t.end) {
                                    Ok(range) => {
                                        t.armed = Some(range);
                                        t.error = None;
                                    }
                                    Err(e) => t.error = Some(e),
                                }
                            }
                        }
                        Some((s, e)) => {
                            ui.label(tf(
                                "Armed: {} – {}",
                                &[&s.format("%a %H:%M"), &e.format("%a %H:%M")],
                            ));
                            if t.running {
                                ui.colored_label(Color32::from_rgb(229, 57, 53), tr("Recording…"));
                            } else {
                                // Time to the start as H:MM:SS.
                                let wait = (s - Local::now()).num_seconds().max(0);
                                ui.label(tf(
                                    "Starts in {}",
                                    &[&format!(
                                        "{}:{:02}:{:02}",
                                        wait / 3600,
                                        wait / 60 % 60,
                                        wait % 60
                                    )],
                                ));
                            }
                            if ui.button(tr("Disarm")).clicked() {
                                t.armed = None;
                                t.running = false;
                            }
                        }
                    }
                    if let Some(e) = &t.error {
                        ui.colored_label(crate::util::error_color(ui), e);
                    }
                    ui.label(
                        RichText::new(tr(
                            "Keep Softromeda Screen Recorder running for the timer to work.",
                        ))
                        .small()
                        .weak(),
                    );
                });
            },
        );
        if close {
            self.timer.open = false;
            crate::util::forget_reveal(ctx, ViewportId::from_hash_of("timer"));
        }
    }

    /// Offers to recover the first interrupted recording found at start-up, in a modal dialog.
    ///
    /// Recovering finalises the session's raw files into a new output file on a background job
    /// (shown like normal saving); discarding deletes the session folder. Only shown while idle;
    /// further sessions are offered one after another.
    fn recovery_ui(&mut self, ui: &mut egui::Ui) {
        if self.recovery.is_empty() || !self.is_idle() {
            return;
        }
        let session = self.recovery[0].clone();
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("recovery")).show(ui.ctx(), |ui| {
            ui.heading(tr("Recover recording?"));
            // Show the creation time as "YYYY-MM-DD HH:MM:SS" (the stored value is RFC 3339).
            ui.label(tf(
                "A recording from {} was interrupted (unexpected shutdown or crash).",
                &[&session
                    .manifest
                    .created
                    .get(..19)
                    .unwrap_or(&session.manifest.created)
                    .replace('T', " ")],
            ));
            ui.horizontal(|ui| {
                if ui.button(tr("Recover")).clicked() {
                    choice = Some(true);
                }
                if ui.button(tr("Discard")).clicked() {
                    choice = Some(false);
                }
            });
        });
        let Some(recover) = choice else { return };
        self.recovery.remove(0);
        if !recover {
            let _ = std::fs::remove_dir_all(&session.dir);
            return;
        }
        // Save as "<name> (recovered).<ext>" in the output folder, never overwriting a file.
        let mut session = session;
        session.manifest.output =
            compose::recovered_output(&session.manifest.settings, &session.manifest.output);
        let job = Job::spawn("Recovering", move |job| {
            let out = compose::finalize(&session, Some(job))?;
            Ok((out, tr("Recovered").into()))
        });
        self.phase = Phase::Finalizing {
            job,
            sleep_after: false,
        };
    }

    #[cfg(windows)]
    /// Hides the main window from screen capture only while it is the recording control bar.
    ///
    /// The bar therefore never appears in recordings, but the window stays visible to
    /// screenshots (Snipping Tool, Win+Shift+S) at other times. Uses
    /// `SetWindowDisplayAffinity`, and only calls it when the desired state changes.
    fn update_capture_exclusion(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let exclude = self.layout == Layout::Bar;
        if self.capture_excluded == Some(exclude) {
            return;
        }
        if let Ok(h) = frame.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{
                    SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
                };
                let affinity = if exclude {
                    WDA_EXCLUDEFROMCAPTURE
                } else {
                    WDA_NONE
                };
                // SAFETY: hwnd is the live root window handle provided by eframe.
                if let Err(e) =
                    unsafe { SetWindowDisplayAffinity(HWND(w.hwnd.get() as _), affinity) }
                {
                    log::warn!("could not change the window's capture visibility: {e}");
                }
                self.capture_excluded = Some(exclude);
            }
        }
    }
}

impl eframe::App for App {
    /// Runs the per-frame logic and window reconfiguration; called even while the main window
    /// is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
        self.apply_layout(ctx);
    }

    /// Draws the main window according to the current [`Layout`], then the secondary windows
    /// (Options, Timer, tools, library and editor).
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(windows)]
        self.update_capture_exclusion(frame);
        #[cfg(not(windows))]
        let _ = frame;
        let ctx = ui.ctx().clone();

        match self.layout {
            Layout::Overlay(_) => {
                // Let the selector draw and handle input; act on its result after the borrow.
                let mut result = None;
                if let Phase::Selecting(sel) = &mut self.phase {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(ui, |ui| match sel.show(ui) {
                            SelectorResult::Pending => {}
                            r => result = Some(r),
                        });
                }
                match result {
                    Some(SelectorResult::Selected(region)) => {
                        // `begin_countdown` expects to start from idle.
                        self.phase = Phase::Idle;
                        self.begin_countdown(Some(window_region(region)));
                    }
                    Some(_) => self.abort_to_idle(),
                    None => {}
                }
                return; // nothing else while selecting
            }
            Layout::Countdown(_) => {
                self.countdown_ui(ui);
                return;
            }
            Layout::Hidden => return,
            Layout::Bar => {
                self.control_bar(ui);
                self.region_border(&ctx);
            }
            Layout::Panel => {
                self.panel_ui(ui);
                self.recovery_ui(ui);
            }
        }

        // Secondary windows are separate viewports, shown on top of the panel or control bar.
        self.options_ui(&ctx);
        self.timer_ui(&ctx);
        self.tools.show(&ctx);
        let dir = self.settings.output_dir.clone();
        if let Some(video) = self.library.show(&ctx, &dir) {
            self.editor.open_file(&video, &ctx);
        }
        self.editor.auto_zoom = self.settings.auto_zoom;
        self.editor.show(&ctx);
    }

    /// Saves a recording still in progress (synchronously, without progress reporting) and the
    /// settings when the application exits.
    fn on_exit(&mut self) {
        if matches!(self.phase, Phase::Recording { .. }) {
            if let Phase::Recording { rec, .. } = std::mem::replace(&mut self.phase, Phase::Idle) {
                if let Ok(session) = rec.stop() {
                    let _ = compose::finalize(&session, None);
                }
            }
        }
        // Never leave the desktop icons hidden.
        if self.icons_hidden {
            crate::util::set_desktop_icons_visible(true);
            self.settings.desktop_icons_hidden = false;
        }
        self.settings.save();
    }
}

// ----------------------------------------------------------------------
// Helpers

/// Applies the application's visual style (text sizes, spacing, colours) to the egui context.
fn setup_style(ctx: &egui::Context) {
    crate::theme::apply(ctx);
}

/// Returns the mouse pointer position in physical desktop pixels, or `None` if it cannot be read.
pub fn mouse_position() -> Option<(i32, i32)> {
    use device_query::{DeviceQuery, DeviceState};
    DeviceState::checked_new().map(|d| d.get_mouse().coords)
}

/// Draws an audio level meter for the control bar: `icon` followed by a horizontal bar.
///
/// `level` is the peak amplitude (0.0–1.0); the bar shows its square root, which makes quiet
/// sounds visible, and turns red near full scale. `fg` is the icon colour and `scale` the bar's
/// size factor.
fn level_meter(ui: &mut egui::Ui, icon: &str, level: f32, fg: Color32, scale: f32) {
    ui.label(RichText::new(icon).color(fg));
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(46.0 * scale, 10.0 * scale), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 2.0, Color32::from_black_alpha(90));
    let lvl = level.clamp(0.0, 1.0).sqrt();
    let filled = Rect::from_min_size(rect.min, Vec2::new(rect.width() * lvl, rect.height()));
    let c = if lvl > 0.9 {
        Color32::from_rgb(229, 57, 53)
    } else {
        Color32::from_rgb(67, 160, 71)
    };
    ui.painter().rect_filled(filled, 2.0, c);
}

/// Draws the choice of whose sound to record: "All programs" (`None`) or one program from
/// `programs` (executable name, window title). A chosen program that is not running now is
/// still listed, so the choice stays visible.
fn program_menu(ui: &mut egui::Ui, programs: &[(String, String)], selected: &mut Option<String>) {
    if ui.radio(selected.is_none(), tr("All programs")).clicked() {
        *selected = None;
    }
    let mut shown = false;
    for (exe, title) in programs {
        let on = selected
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(exe));
        shown |= on;
        let label = format!("{} — {}", exe.trim_end_matches(".exe"), short(title, 40));
        if ui.radio(on, label).clicked() {
            *selected = Some(exe.clone());
        }
    }
    if let (Some(exe), false) = (selected.clone(), shown) {
        let _ = ui.radio(true, tf("{} (not running)", &[&exe]));
    }
}

/// `text` cut to at most `max` characters, with an ellipsis when shortened.
fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

/// Draws device radio buttons for a chip's context menu: "System default" (`None`) and every
/// device in `list`, selected by name.
fn device_menu(ui: &mut egui::Ui, list: &[String], selected: &mut Option<String>) {
    if ui.radio(selected.is_none(), tr("System default")).clicked() {
        *selected = None;
    }
    for d in list {
        if ui.radio(selected.as_deref() == Some(d), d).clicked() {
            *selected = Some(d.clone());
        }
    }
    if list.is_empty() {
        ui.label(RichText::new(tr("No devices found")).weak());
    }
}

/// Draws a device drop-down for the Options dialog: "System default" (`None`) and every device in
/// `list`. `id` must be unique among the combo boxes on the page.
fn device_combo(ui: &mut egui::Ui, id: &str, list: &[String], selected: &mut Option<String>) {
    egui::ComboBox::from_id_salt(id)
        .width(360.0)
        .selected_text(
            selected
                .clone()
                .unwrap_or_else(|| tr("System default").into()),
        )
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, tr("System default"));
            for d in list {
                ui.selectable_value(selected, Some(d.clone()), d);
            }
        });
}

/// Draws the drop-down for what happens after a recording is saved.
fn after_recording_combo(ui: &mut egui::Ui, v: &mut AfterRecording) {
    let label = |a: AfterRecording| match a {
        AfterRecording::OpenEditor => tr("Open in video editor"),
        AfterRecording::OpenFolder => tr("Show in folder"),
        AfterRecording::DoNothing => tr("Do nothing"),
    };
    egui::ComboBox::from_id_salt("after-rec")
        .selected_text(label(*v))
        .show_ui(ui, |ui| {
            for a in [
                AfterRecording::OpenEditor,
                AfterRecording::OpenFolder,
                AfterRecording::DoNothing,
            ] {
                ui.selectable_value(v, a, label(a));
            }
        });
}

/// Draws the second row of the control bar: drawing tools, colours, undo and clear.
///
/// Choosing a tool or colour also turns drawing on. `scale` is the control bar's size factor.
fn draw_tools(ui: &mut egui::Ui, o: &crate::overlay::DrawOverlay, scale: f32) {
    use crate::overlay::{COLORS, DrawTool};
    ui.horizontal(|ui| {
        ui.set_min_height(32.0 * scale);
        ui.spacing_mut().item_spacing.x = 4.0 * scale;
        let tool = o.tool();
        for t in DrawTool::ALL {
            let b = egui::Button::new(RichText::new(t.icon()).size(15.0 * scale))
                .selected(tool == t)
                .min_size(Vec2::splat(26.0 * scale));
            if ui.add(b).on_hover_text(t.label()).clicked() {
                o.set_tool(t);
                o.set_drawing(true);
            }
        }
        ui.add_space(8.0 * scale);
        // Colour swatches as clickable circles; the current colour gets a thick white ring.
        let current = o.color();
        for (i, c) in COLORS.iter().enumerate() {
            let size = Vec2::splat(20.0 * scale);
            let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
            let color = Color32::from_rgb(c[0], c[1], c[2]);
            ui.painter()
                .circle_filled(rect.center(), 8.0 * scale, color);
            let ring = if i == current {
                egui::Stroke::new(2.5 * scale, Color32::WHITE)
            } else {
                egui::Stroke::new(1.0, Color32::from_gray(120))
            };
            ui.painter().circle_stroke(rect.center(), 9.5 * scale, ring);
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                o.set_color(i);
                o.set_drawing(true);
            }
        }
        ui.add_space(8.0 * scale);
        if ui
            .add(egui::Button::new(
                RichText::new(tr("↺ Undo")).size(13.0 * scale),
            ))
            .on_hover_text("Ctrl+Z")
            .clicked()
        {
            o.undo();
        }
        if ui
            .add(egui::Button::new(
                RichText::new(tr("🗑 Clear")).size(13.0 * scale),
            ))
            .on_hover_text("Delete")
            .clicked()
        {
            o.clear();
        }
    });
}

/// Adjusts a region chosen in the selector to the window it refers to, if any.
///
/// For a chosen window, its visible bounds (without the invisible resize borders) become the
/// recorded size, because window capture delivers exactly that; the region is assigned to the
/// monitor containing the window's centre. If the window has gone (or none was chosen), the
/// region is recorded as a plain area. Width and height are at least 16 pixels.
fn window_region(mut r: CaptureRegion) -> CaptureRegion {
    let Some((x, y, w, h)) = r.window.and_then(crate::media::wgc::window_bounds) else {
        r.window = None;
        return r;
    };
    let (cx, cy) = (x + w as i32 / 2, y + h as i32 / 2);
    if let Some(m) = geometry::monitor_at(cx, cy) {
        r.monitor_id = m.id;
        r.x = (x - m.x).max(0) as u32;
        r.y = (y - m.y).max(0) as u32;
    }
    r.width = w.max(16);
    r.height = h.max(16);
    r
}

/// Horizontal padding, in logical points, between the control bar's edges and its buttons.
const BAR_MARGIN_X: f32 = 10.0;

/// Returns the control bar's starting size in logical points: one row, or two when the drawing
/// tools are shown, scaled by the control bar size setting. The width is then fitted to the
/// buttons ([`App::fit_bar_width`]).
fn bar_size(s: &Settings, draw_row: bool) -> Vec2 {
    let k = s.control_bar_scale.clamp(0.7, 2.0);
    Vec2::new(680.0 * k, if draw_row { 80.0 } else { 40.0 } * k)
}

/// Returns the control bar's top-left position in logical points, just outside the recorded area
/// when possible so that it is not captured.
///
/// The bar goes below the area if it fits (leaving room for the taskbar), otherwise above it,
/// otherwise at the top centre of the monitor; audio-only recordings also use the top centre.
fn bar_position(region: Option<CaptureRegion>, size: Vec2) -> Pos2 {
    let mon = region
        .and_then(|r| {
            geometry::list_monitors()
                .into_iter()
                .find(|m| m.id == r.monitor_id)
        })
        .or_else(geometry::primary_monitor);
    let Some(mon) = mon else {
        return Pos2::new(40.0, 40.0);
    };
    let m = mon.logical_rect();
    let Some(r) = region else {
        return Pos2::new(m.center().x - size.x / 2.0, m.top() + 8.0);
    };
    // The recorded area in logical desktop points, and the bar's x centred on it but kept on
    // the monitor.
    let upp = mon.units_per_point();
    let rr = Rect::from_min_size(
        m.min + Vec2::new(r.x as f32 / upp, r.y as f32 / upp),
        Vec2::new(r.width as f32 / upp, r.height as f32 / upp),
    );
    let x = (rr.center().x - size.x / 2.0).clamp(m.left(), (m.right() - size.x).max(m.left()));
    if rr.bottom() + 8.0 + size.y < m.bottom() - 40.0 {
        Pos2::new(x, rr.bottom() + 8.0)
    } else if rr.top() - 8.0 - size.y > m.top() {
        Pos2::new(x, rr.top() - 8.0 - size.y)
    } else {
        Pos2::new(m.center().x - size.x / 2.0, m.top() + 8.0)
    }
}

/// Converts `HH:MM` start and stop times into the next occurrence of the start time and the
/// first occurrence of the stop time after it.
///
/// A start time later today, or less than a minute ago, counts as today, an earlier one as
/// tomorrow; a stop time not after the start falls on the following day. Returns a translated
/// error message for an invalid time.
fn schedule(
    start: &str,
    end: &str,
) -> Result<(chrono::DateTime<Local>, chrono::DateTime<Local>), String> {
    let parse = |s: &str| {
        NaiveTime::parse_from_str(s.trim(), "%H:%M")
            .map_err(|_| tf("'{}' is not a valid HH:MM time", &[&s]))
    };
    let (st, et) = (parse(start)?, parse(end)?);
    let now = Local::now();
    let today = now.date_naive();
    let mut s = Local
        .from_local_datetime(&today.and_time(st))
        .single()
        .ok_or(tr("invalid start time"))?;
    if s + chrono::Duration::seconds(59) < now {
        s += chrono::Duration::days(1);
    }
    let mut e = Local
        .from_local_datetime(&s.date_naive().and_time(et))
        .single()
        .ok_or(tr("invalid end time"))?;
    if e <= s {
        e += chrono::Duration::days(1);
    }
    Ok((s, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stop time before the start time wraps to the next day; invalid times are rejected.
    #[test]
    fn schedule_end_after_start() {
        let (s, e) = schedule("23:50", "00:10").unwrap();
        assert!(e > s);
        assert!((e - s).num_minutes() == 20);
        assert!(schedule("25:00", "01:00").is_err());
    }
}
