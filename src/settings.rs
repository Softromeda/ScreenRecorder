// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Persistent user settings.
//!
//! [`Settings`] holds every user-configurable option (output, audio, webcam, mouse and keyboard
//! effects, recording behaviour, control bar, hotkeys and appearance). It is loaded once at
//! start-up, edited in the Options window and saved as pretty-printed JSON in the per-user config
//! directory. A copy is stored in each recording's session manifest so that the recorder and
//! [`crate::compose`] use the options that were in effect when the recording started.

use std::path::PathBuf;

use crate::i18n::tr;
use serde::{Deserialize, Serialize};

use crate::geometry::CaptureRegion;

/// Container and codec of the final recording file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Animated GIF without sound.
    Gif,
    /// Audio-only recording encoded as MP3.
    Mp3,
    /// MP4 with H.264 video and AAC audio.
    ///
    /// Also the fallback for any unrecognised value in a settings file (e.g. `"WebM"` or
    /// `"MKV"`), via `#[serde(other)]`.
    #[serde(other)]
    Mp4,
}

impl OutputFormat {
    /// All formats in the order they are offered in the UI.
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Mp4, OutputFormat::Gif, OutputFormat::Mp3];

    /// File name extension, without the leading dot.
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Gif => "gif",
            OutputFormat::Mp3 => "mp3",
        }
    }

    /// Translated name shown in the format picker.
    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => tr("MP4 (H.264 + AAC)"),
            OutputFormat::Gif => tr("Animated GIF"),
            OutputFormat::Mp3 => tr("MP3 (audio only)"),
        }
    }

    /// Whether the format records sound only (no screen capture).
    pub fn is_audio_only(self) -> bool {
        matches!(self, OutputFormat::Mp3)
    }

    /// Whether the format can carry an audio track (GIF cannot).
    pub fn has_audio(self) -> bool {
        !matches!(self, OutputFormat::Gif)
    }
}

/// Colour scheme of the user interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    /// Always use the light scheme.
    Light,
    /// Always use the dark scheme.
    Dark,
}

impl Theme {
    /// All themes in the order they are offered in the UI.
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    /// Translated name shown in the theme picker.
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => tr("Same as system"),
            Theme::Light => tr("Light"),
            Theme::Dark => tr("Dark"),
        }
    }

    /// The equivalent egui theme preference, applied to the egui context.
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Theme::System => egui::ThemePreference::System,
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// What the program does once a recording has been saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AfterRecording {
    /// Open the new file in the built-in video editor.
    OpenEditor,
    /// Show the file in Windows Explorer.
    OpenFolder,
    /// Stay in the capture panel.
    DoNothing,
}

/// How strongly microphone noise reduction works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoiseStrength {
    /// Gentle: keeps the voice most natural, removes less noise.
    Light,
    /// Balanced for most rooms and microphones.
    Medium,
    /// Removes the most noise; soft consonants may sound thinner.
    Strong,
}

impl NoiseStrength {
    /// All strengths, in the order shown in the options.
    pub const ALL: [NoiseStrength; 3] = [
        NoiseStrength::Light,
        NoiseStrength::Medium,
        NoiseStrength::Strong,
    ];

    /// Translated name for the options.
    pub fn label(self) -> &'static str {
        match self {
            NoiseStrength::Light => tr("Gentle"),
            NoiseStrength::Medium => tr("Medium"),
            NoiseStrength::Strong => tr("Strong"),
        }
    }

    /// Over-subtraction factor and maximum reduction in dB for
    /// [`denoise`](crate::media::voice::denoise).
    pub fn params(self) -> (f32, f32) {
        match self {
            NoiseStrength::Light => (2.0, 10.0),
            NoiseStrength::Medium => (3.0, 15.0),
            NoiseStrength::Strong => (4.0, 22.0),
        }
    }
}

/// Corner of the video in which the picture-in-picture webcam overlay is placed.
///
/// Also used for the corner of the editor's logo watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebcamPosition {
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner (the default for the webcam).
    BottomRight,
}

impl WebcamPosition {
    /// All corners in the order they are offered in the UI.
    pub const ALL: [WebcamPosition; 4] = [
        WebcamPosition::TopLeft,
        WebcamPosition::TopRight,
        WebcamPosition::BottomLeft,
        WebcamPosition::BottomRight,
    ];

    /// Translated name shown in the position picker.
    pub fn label(self) -> &'static str {
        match self {
            WebcamPosition::TopLeft => tr("Top left"),
            WebcamPosition::TopRight => tr("Top right"),
            WebcamPosition::BottomLeft => tr("Bottom left"),
            WebcamPosition::BottomRight => tr("Bottom right"),
        }
    }
}

/// A colour stored as `[red, green, blue, alpha]` bytes (straight, not premultiplied, alpha).
pub type Rgba = [u8; 4];

/// All user-configurable options.
///
/// `#[serde(default)]` lets a settings file that lacks some fields load anyway: missing fields
/// take their values from [`Settings::default`]. Settings files written by older builds
/// therefore keep working, so a new field only needs a value in [`Settings::default`]. Renaming
/// or removing a field, or changing what an existing value means, needs a step in
/// `Settings::migrate` and a bump of `SETTINGS_VERSION`. Unknown fields in a file are ignored.
///
/// The `// ----` section comments group related fields; the order of fields has no effect on
/// loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ---- Output ----
    /// Folder that new recordings are saved to.
    pub output_dir: PathBuf,
    /// `strftime`-style template (expanded with the local time) for automatic file names.
    pub filename_template: String,
    /// Format of the final file.
    pub format: OutputFormat,
    /// Capture and output frame rate, in frames per second.
    pub fps: u32,
    /// Encoding quality from 1 (smallest file) to 100 (best picture).
    pub quality: u32,
    /// Output scale in percent of the captured size (25..=100).
    pub scale_percent: u32,
    /// Action taken after a recording has been saved.
    pub after_recording: AfterRecording,

    // ---- Audio ----
    /// Record the microphone.
    pub record_microphone: bool,
    /// Record the speakers (system audio, via WASAPI loopback).
    pub record_speakers: bool,
    /// Microphone device name; `None` means the system default.
    pub microphone_device: Option<String>,
    /// Speaker (output) device name; `None` means the system default.
    pub speaker_device: Option<String>,
    /// Record only this program's sound (its executable name, such as `chrome.exe`);
    /// `None` records the sound of all programs.
    pub speaker_program: Option<String>,
    /// Linear gain applied to the microphone while recording (1.0 = unchanged, up to 3.0).
    pub microphone_volume: f32,
    /// Linear gain applied to the speaker capture while recording (1.0 = unchanged).
    pub speaker_volume: f32,
    /// Raise a quiet speaker track automatically when the recording is saved.
    pub auto_boost_speakers: bool,

    // ---- Webcam ----
    /// Add a picture-in-picture webcam overlay to the video.
    pub record_webcam: bool,
    /// Webcam device name; `None` means the first camera found.
    pub webcam_device: Option<String>,
    /// Corner in which the webcam overlay is drawn.
    pub webcam_position: WebcamPosition,
    /// Webcam overlay width as a percentage of the video width.
    pub webcam_size_percent: u32,

    // ---- Mouse & keyboard ----
    /// Draw the mouse pointer into the video.
    pub show_cursor: bool,
    /// Draw a translucent disc behind the pointer.
    pub highlight_cursor: bool,
    /// Colour of the pointer highlight.
    pub highlight_color: Rgba,
    /// Radius of the pointer highlight, in pixels of the captured image (10..=80 in the UI).
    pub highlight_radius: f32,
    // ---- Updates and desktop ----
    /// Look for a newer version on GitHub (at most once a day).
    pub check_updates: bool,
    /// When updates were last looked for (Unix time in seconds; 0 = never).
    pub last_update_check: i64,
    /// A newer version the user chose to skip (not offered again).
    pub skipped_version: Option<String>,
    /// Hide the desktop icons during the countdown and recording.
    pub hide_desktop_icons: bool,
    /// The icons are hidden right now (so they can be restored after a crash).
    pub desktop_icons_hidden: bool,
    // ---- Mouse & keyboard (continued) ----
    /// Draw the pointer with smoothed movement (removes shake).
    pub smooth_cursor: bool,
    /// Dim everything except a circle around the mouse pointer.
    pub spotlight: bool,
    /// Radius of the spotlight circle, in pixels of the captured image at 100 % scaling.
    pub spotlight_radius: f32,
    /// How much the area outside the spotlight is darkened (0..=1).
    pub spotlight_dim: f32,
    /// Draw an expanding ripple at each mouse click.
    pub show_clicks: bool,
    /// Ripple colour for left clicks.
    pub left_click_color: Rgba,
    /// Ripple colour for right clicks.
    pub right_click_color: Rgba,
    /// Mix a click sound into the audio at each left click (added when the recording is saved).
    pub click_sound: bool,
    /// Show pressed keys and typed text as an on-screen caption.
    pub show_keystrokes: bool,

    // ---- Recording behaviour ----
    /// Seconds of countdown before recording starts (0 = start immediately).
    pub countdown_seconds: u32,
    /// Draw a border around the recorded area while recording.
    pub show_region_border: bool,
    /// Hide the capture panel while recording, leaving only the control bar.
    pub hide_panel_while_recording: bool,
    /// Stop automatically after this many minutes (0 = unlimited).
    pub max_duration_minutes: u32,
    /// Width, in pixels, of the "Fixed-size Region" capture mode.
    pub fixed_region_width: u32,
    /// Height, in pixels, of the "Fixed-size Region" capture mode.
    pub fixed_region_height: u32,
    /// Region of the most recent recording, for "Repeat Last Region".
    pub last_region: Option<CaptureRegion>,

    // ---- Control bar ----
    /// Background colour of the floating control bar.
    pub control_bar_color: Rgba,
    /// Size multiplier of the control bar (clamped to 0.7..=2.0 when used).
    pub control_bar_scale: f32,

    // ---- Hotkeys ----
    /// Global hotkey that starts, pauses and resumes recording (e.g. `"Ctrl+F11"`).
    pub hotkey_start_pause: String,
    /// Global hotkey that stops and saves the recording.
    pub hotkey_stop: String,
    /// Global hotkey that discards the recording.
    pub hotkey_cancel: String,
    /// Global hotkey that toggles drawing on screen.
    pub hotkey_draw: String,
    /// Hotkey that drops a marker while recording.
    pub hotkey_marker: String,
    // ---- Editor, audio processing and tray ----
    /// Create zoom effects around mouse clicks when a new recording opens in the editor.
    pub auto_zoom: bool,
    /// Reduce steady background noise (fans, hum, hiss) in the microphone track when saving.
    pub mic_noise_reduction: bool,
    /// How strongly background noise is removed.
    pub noise_strength: NoiseStrength,
    /// Even out the microphone speech level, without clipping, when saving.
    pub mic_leveling: bool,
    /// Show an icon in the notification area (system tray).
    pub tray_icon: bool,
    /// Closing the main window keeps the program running in the tray.
    pub close_to_tray: bool,
    /// Start (in the tray) when the user signs in to Windows.
    pub start_with_windows: bool,

    // ---- Misc ----
    /// Optional TTF/OTF font used for keystrokes, captions and title pages; useful for scripts
    /// the built-in font lacks, such as Korean. Empty means the built-in font.
    pub custom_font_path: String,
    /// Show the recording settings expanded in the capture panel (larger panel).
    pub detailed_mode: bool,
    /// Colour scheme of the interface.
    pub theme: Theme,
    /// Interface language (`None` = same as Windows, falling back to English).
    #[serde(default)]
    pub language: Option<crate::i18n::Language>,
    /// Format version of the saved settings (0 = file written before versioning existed).
    ///
    /// The field-level `#[serde(default)]` matters here: it makes a missing value 0 (the type's
    /// default) instead of the current version from [`Settings::default`], so that files written
    /// before versioning are still migrated.
    #[serde(default)]
    pub settings_version: u32,
}

/// Format version written by this build; see [`Settings::settings_version`].
const SETTINGS_VERSION: u32 = 2;

impl Default for Settings {
    /// Defaults for a first run: MP4 at 25 fps saved to `Videos\<app name>`, no audio or webcam,
    /// cursor and clicks shown, a 3-second countdown and the standard hotkeys.
    fn default() -> Self {
        // Prefer the user's Videos folder, then the home folder, then the working directory.
        let output_dir = directories::UserDirs::new()
            .and_then(|d| d.video_dir().map(|p| p.to_path_buf()))
            .or_else(|| directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(crate::util::APP_NAME);
        Self {
            output_dir,
            filename_template: "Recording_%Y-%m-%d_%H-%M-%S".into(),
            format: OutputFormat::Mp4,
            fps: 25,
            quality: 70,
            scale_percent: 100,
            after_recording: AfterRecording::OpenEditor,

            record_microphone: false,
            record_speakers: false,
            microphone_device: None,
            speaker_device: None,
            speaker_program: None,
            microphone_volume: 1.0,
            speaker_volume: 1.0,
            auto_boost_speakers: true,

            record_webcam: false,
            webcam_device: None,
            webcam_position: WebcamPosition::BottomRight,
            webcam_size_percent: 15,

            show_cursor: true,
            highlight_cursor: false,
            highlight_color: [255, 235, 59, 110],
            highlight_radius: 28.0,
            check_updates: true,
            last_update_check: 0,
            skipped_version: None,
            hide_desktop_icons: false,
            desktop_icons_hidden: false,
            smooth_cursor: true,
            spotlight: false,
            spotlight_radius: 160.0,
            spotlight_dim: 0.55,
            show_clicks: true,
            left_click_color: [244, 67, 54, 220],
            right_click_color: [33, 150, 243, 220],
            click_sound: false,
            show_keystrokes: false,

            countdown_seconds: 3,
            show_region_border: true,
            hide_panel_while_recording: false,
            max_duration_minutes: 0,
            fixed_region_width: 1280,
            fixed_region_height: 720,
            last_region: None,

            control_bar_color: [255, 214, 0, 255],
            control_bar_scale: 1.0,

            hotkey_start_pause: "Ctrl+F11".into(),
            hotkey_stop: "Ctrl+F12".into(),
            hotkey_cancel: "Ctrl+Shift+F12".into(),
            hotkey_draw: "Ctrl+F10".into(),
            hotkey_marker: "Ctrl+F9".into(),
            auto_zoom: false,
            mic_noise_reduction: true,
            noise_strength: NoiseStrength::Medium,
            mic_leveling: true,
            tray_icon: true,
            close_to_tray: false,
            start_with_windows: false,

            custom_font_path: String::new(),
            detailed_mode: false,
            theme: Theme::System,
            language: None,
            settings_version: SETTINGS_VERSION,
        }
    }
}

impl Settings {
    /// Per-user configuration directory holding `settings.json`.
    ///
    /// Falls back to `.softromeda-recorder` in the working directory if no home directory is known.
    pub fn config_dir() -> PathBuf {
        crate::util::project_dirs()
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".softromeda-recorder"))
    }

    /// Directory holding in-progress recording sessions, one subfolder each.
    ///
    /// Sessions stay here until they are composed into the final file, so an interrupted
    /// recording can be found and recovered on the next start.
    pub fn sessions_dir() -> PathBuf {
        crate::util::project_dirs()
            .map(|d| d.data_local_dir().join("sessions"))
            .unwrap_or_else(|| std::env::temp_dir().join("softromeda-recorder-sessions"))
    }

    /// Path of the settings file.
    fn file() -> PathBuf {
        Self::config_dir().join("settings.json")
    }

    /// Loads the saved settings and brings them up to the current format version.
    ///
    /// A missing or unreadable file yields the defaults; errors are not reported.
    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(Self::file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        s.migrate();
        s
    }

    /// Upgrades settings written with an older [`Settings::settings_version`] and stamps the
    /// current version.
    fn migrate(&mut self) {
        if self.settings_version < 2 {
            // Version 1 files used 22% as the default webcam size; map that value to the current
            // 15% default while keeping any other size the user chose.
            if self.webcam_size_percent == 22 {
                self.webcam_size_percent = 15;
            }
        }
        self.settings_version = SETTINGS_VERSION;
    }

    /// Writes the settings to disk, creating the config directory if needed.
    ///
    /// Failures are logged as warnings and otherwise ignored, so a read-only profile never
    /// interrupts the user.
    pub fn save(&self) {
        let path = Self::file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("could not save settings: {e}");
                }
            }
            Err(e) => log::warn!("could not serialise settings: {e}"),
        }
    }

    /// Builds the path for the next recording from the file name template and the current time.
    ///
    /// Characters not allowed in Windows file names are replaced, and ` (2)`, ` (3)`, … is
    /// appended until the name does not clash with an existing file.
    pub fn next_output_path(&self) -> PathBuf {
        let name = chrono::Local::now()
            .format(&self.filename_template)
            .to_string();
        let name = sanitize_filename(&name);
        let ext = self.format.extension();
        let mut path = self.output_dir.join(format!("{name}.{ext}"));
        // Number the name rather than overwrite an earlier recording.
        let mut n = 2;
        while path.exists() {
            path = self.output_dir.join(format!("{name} ({n}).{ext}"));
            n += 1;
        }
        path
    }

    /// All configured hotkey strings paired with their actions, for global registration.
    pub fn hotkeys(&self) -> [(&str, crate::hotkeys::HotkeyAction); 5] {
        use crate::hotkeys::HotkeyAction;
        [
            (&self.hotkey_start_pause, HotkeyAction::StartPause),
            (&self.hotkey_stop, HotkeyAction::Stop),
            (&self.hotkey_cancel, HotkeyAction::Cancel),
            (&self.hotkey_draw, HotkeyAction::Draw),
            (&self.hotkey_marker, HotkeyAction::Marker),
        ]
    }

    /// Contents of the custom font file, or `None` if no path is set or it cannot be read.
    pub fn font_bytes(&self) -> Option<Vec<u8>> {
        if self.custom_font_path.trim().is_empty() {
            return None;
        }
        std::fs::read(self.custom_font_path.trim()).ok()
    }
}

/// Replaces each character that Windows forbids in file names (`/ \ : * ? " < > |`) with `_`.
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forbidden file name characters are replaced with underscores.
    #[test]
    fn sanitize() {
        assert_eq!(sanitize_filename("a/b:c"), "a_b_c");
    }

    /// Partial files load with defaults, version migration only touches old default values, and
    /// unknown output formats load as MP4.
    #[test]
    fn settings_roundtrip_with_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"fps": 10}"#).unwrap();
        assert_eq!(s.fps, 10);
        assert_eq!(s.format, OutputFormat::Mp4);
        // A version-1 default webcam size is migrated; a custom size, or 22% in a current
        // file, is kept.
        let mut old: Settings = serde_json::from_str(r#"{"webcam_size_percent": 22}"#).unwrap();
        old.migrate();
        assert_eq!(old.webcam_size_percent, 15);
        let mut custom: Settings = serde_json::from_str(r#"{"webcam_size_percent": 30}"#).unwrap();
        custom.migrate();
        assert_eq!(custom.webcam_size_percent, 30);
        let mut new: Settings =
            serde_json::from_str(r#"{"webcam_size_percent": 22, "settings_version": 2}"#).unwrap();
        new.migrate();
        assert_eq!(new.webcam_size_percent, 22);
        // Unknown format names fall back to MP4.
        let s: Settings = serde_json::from_str(r#"{"format": "WebM", "fps": 30}"#).unwrap();
        assert_eq!((s.format, s.fps), (OutputFormat::Mp4, 30));
    }
}
