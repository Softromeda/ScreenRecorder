// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! The recording engine.
//!
//! [`Recorder`] runs one recording session and writes its raw material into a session directory
//! under [`Settings::sessions_dir`]:
//!
//! * `video.mp4`    – screen video (H.264 in fragmented MP4, so it survives a crash)
//! * `mic.wav`      – microphone audio
//! * `speakers.wav` – system audio (WASAPI loopback)
//! * `session.json` – the [`SessionManifest`], also used for crash recovery
//! * `clicks.json`, `mouse.json` – mouse clicks and positions for the editor
//!
//! Video pipeline: a capture thread keeps the newest screen image (window capture, DXGI Desktop
//! Duplication or GDI polling); a pacer thread turns it into exactly `fps` frames per second of
//! active recording time, drawing cursor, click and keystroke effects and the webcam overlay; an
//! encoder thread converts the frames to NV12 and encodes them with Media Foundation (hardware
//! H.264 when available). When the recording stops, [`crate::compose`] adds the audio and
//! produces the final file.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ab_glyph::FontArc;
use anyhow::{Context, Result, anyhow};
use device_query::{DeviceQuery, DeviceState, Keycode};
use serde::{Deserialize, Serialize};
use tiny_skia::{IntSize, Pixmap};

use crate::audio::{AudioCapture, AudioSource, Level};
use crate::clock::ActiveClock;
use crate::draw;
use crate::geometry::{CaptureRegion, MonitorInfo, find_monitor};
use crate::media::capture::{Duplicator, Grab};
use crate::media::{convert, mf};
use crate::settings::{Settings, WebcamPosition};

/// Description of a recording session, saved as `session.json` in the session directory.
///
/// It is written when recording starts and again when it stops, so that a recording interrupted
/// by a crash or power loss can be found and composed on the next start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    /// Path of the final file that [`crate::compose`] produces.
    pub output: PathBuf,
    /// Settings in effect when the recording started.
    pub settings: Settings,
    /// Recorded area, or `None` for an audio-only recording.
    pub region: Option<CaptureRegion>,
    /// Whether a screen video is being recorded.
    pub has_video: bool,
    /// Whether the microphone was opened successfully.
    pub has_mic: bool,
    /// Whether the speaker loopback was opened successfully.
    pub has_speakers: bool,
    /// Local start time in RFC 3339 format.
    pub created: String,
    /// Active (un-paused) duration in seconds; 0 if the recording crashed.
    pub duration: f64,
    /// Set when the recording was stopped normally and all raw files were finalised.
    pub finished: bool,
}

impl SessionManifest {
    /// Path of the raw screen video in session directory `dir`.
    pub fn video_path(dir: &Path) -> PathBuf {
        dir.join("video.mp4")
    }
    /// Path of the raw microphone recording in session directory `dir`.
    pub fn mic_path(dir: &Path) -> PathBuf {
        dir.join("mic.wav")
    }
    /// Path of the raw speaker (system audio) recording in session directory `dir`.
    pub fn speakers_path(dir: &Path) -> PathBuf {
        dir.join("speakers.wav")
    }
    /// Path of the manifest file in session directory `dir`.
    pub fn file(dir: &Path) -> PathBuf {
        dir.join("session.json")
    }

    /// Writes the manifest as pretty-printed JSON into session directory `dir`.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(Self::file(dir), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Reads the manifest from session directory `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(
            Self::file(dir),
        )?)?)
    }
}

/// A stopped (or recovered) session whose raw files are ready to be composed.
#[derive(Debug, Clone)]
pub struct FinishedSession {
    /// The session directory holding the raw files.
    pub dir: PathBuf,
    /// The session's manifest.
    pub manifest: SessionManifest,
}

/// Lists the sessions left in the sessions directory, i.e. recordings that were never composed
/// (typically because of a crash or power loss).
///
/// Directories without a readable manifest are skipped.
pub fn find_unfinished_sessions() -> Vec<FinishedSession> {
    let root = Settings::sessions_dir();
    let Ok(rd) = std::fs::read_dir(&root) else {
        return vec![];
    };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|dir| {
            let manifest = SessionManifest::load(&dir).ok()?;
            Some(FinishedSession { dir, manifest })
        })
        .collect()
}

/// A running recording: owns the audio captures and the video pipeline thread.
///
/// Created by [`Recorder::start`]; ended by [`Recorder::stop`] (keep) or [`Recorder::cancel`]
/// (discard). Pausing pauses the shared [`ActiveClock`], which every capture thread follows.
pub struct Recorder {
    /// Active-time clock shared with the video and audio threads.
    pub clock: ActiveClock,
    /// Session directory holding the raw files.
    pub dir: PathBuf,
    /// Manifest, saved again with the final duration when the recording stops.
    manifest: SessionManifest,
    /// Tells the video pacer to finish.
    stop_flag: Arc<AtomicBool>,
    /// The video pipeline (`video_loop`), or `None` for audio-only recordings or once joined.
    video_thread: Option<JoinHandle<Result<()>>>,
    /// Number of frame slots written so far, updated by the pacer.
    frames: Arc<AtomicU64>,
    /// Microphone capture, if requested and opened.
    mic: Option<AudioCapture>,
    /// Speaker loopback capture, if requested and opened.
    speakers: Option<AudioCapture>,
    /// Non-fatal problems found while starting (e.g. an audio device that could not be opened),
    /// for display to the user.
    pub warnings: Vec<String>,
    /// Live microphone level for the control bar meter.
    pub mic_level: Level,
    /// Live speaker level for the control bar meter.
    pub speaker_level: Level,
    /// Markers dropped during the recording (recording time in seconds, pauses excluded).
    markers: Vec<f64>,
}

impl Recorder {
    /// Starts recording `region`, or audio only when the output format is audio-only.
    ///
    /// Creates a new session directory, opens the requested audio devices, saves the manifest
    /// and spawns the video pipeline. An audio device that fails to open only adds a warning,
    /// unless the recording is audio-only and no device could be opened at all. Fails when no
    /// region is given for a video format or the session directory cannot be created.
    pub fn start(settings: &Settings, region: Option<CaptureRegion>) -> Result<Self> {
        // Validate the combination of format, audio sources and region.
        let audio_only = settings.format.is_audio_only();
        if audio_only && !settings.record_microphone && !settings.record_speakers {
            return Err(anyhow!(
                "MP3 is an audio-only format: enable the microphone and/or speakers first."
            ));
        }
        let region = if audio_only { None } else { region };
        if !audio_only && region.is_none() {
            return Err(anyhow!("No capture region selected"));
        }

        // One directory per session, named after the start time (milliseconds keep it unique).
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        let dir = Settings::sessions_dir().join(stamp);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let clock = ActiveClock::new();
        let mut warnings = Vec::new();

        // Open the audio sources; a failure is reported as a warning rather than an error so
        // that the screen is still recorded.
        let wants_audio = settings.format.has_audio();
        let mut mic = None;
        if wants_audio && settings.record_microphone {
            match AudioCapture::start(
                AudioSource::Microphone,
                settings.microphone_device.as_deref(),
                &SessionManifest::mic_path(&dir),
                clock.clone(),
                settings.microphone_volume,
            ) {
                Ok(c) => mic = Some(c),
                Err(e) => warnings.push(format!("Microphone: {e:#}")),
            }
        }
        let mut speakers = None;
        // One program's sound only, when chosen and that program is running.
        if wants_audio
            && settings.record_speakers
            && let Some(exe) = &settings.speaker_program
        {
            let started = crate::media::apploop::find_program(exe)
                .ok_or_else(|| anyhow!("{exe} is not running"))
                .and_then(|pid| {
                    AudioCapture::start_program(
                        pid,
                        &SessionManifest::speakers_path(&dir),
                        clock.clone(),
                        settings.speaker_volume,
                    )
                });
            match started {
                Ok(c) => speakers = Some(c),
                Err(e) => warnings.push(format!(
                    "Speakers: {e:#}. Recording the sound of all programs instead."
                )),
            }
        }
        if wants_audio && settings.record_speakers && speakers.is_none() {
            match AudioCapture::start(
                AudioSource::Speakers,
                settings.speaker_device.as_deref(),
                &SessionManifest::speakers_path(&dir),
                clock.clone(),
                settings.speaker_volume,
            ) {
                Ok(c) => speakers = Some(c),
                Err(e) => warnings.push(format!("Speakers: {e:#}")),
            }
        }
        // An audio-only recording without any audio is pointless: clean up and fail.
        if audio_only && mic.is_none() && speakers.is_none() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(anyhow!(
                "No audio device could be opened:\n{}",
                warnings.join("\n")
            ));
        }

        // Save the manifest before any data arrives so a crash leaves a recoverable session.
        let manifest = SessionManifest {
            output: settings.next_output_path(),
            settings: settings.clone(),
            region,
            has_video: region.is_some(),
            has_mic: mic.is_some(),
            has_speakers: speakers.is_some(),
            created: chrono::Local::now().to_rfc3339(),
            duration: 0.0,
            finished: false,
        };
        manifest.save(&dir)?;

        // Spawn the video pipeline; it waits for the clock to start before producing frames.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let video_thread = if let Some(region) = region {
            let ctx = VideoContext {
                region,
                settings: settings.clone(),
                out: SessionManifest::video_path(&dir),
                clock: clock.clone(),
                stop: stop_flag.clone(),
                frames: frames.clone(),
            };
            Some(
                std::thread::Builder::new()
                    .name("video-pacer".into())
                    .spawn(move || video_loop(ctx))?,
            )
        } else {
            None
        };

        // Silent default meters for sources that are not recorded.
        let mic_level = mic.as_ref().map(|m| m.level.clone()).unwrap_or_default();
        let speaker_level = speakers
            .as_ref()
            .map(|m| m.level.clone())
            .unwrap_or_default();

        let rec = Self {
            clock,
            dir,
            manifest,
            stop_flag,
            video_thread,
            frames,
            mic,
            speakers,
            warnings,
            mic_level,
            speaker_level,
            markers: vec![],
        };
        // Start the shared clock last, so all sources begin on the same timeline at zero.
        rec.clock.start();
        Ok(rec)
    }

    /// Whether the recording is paused.
    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }

    /// Pauses the recording; does nothing if it is not running.
    pub fn pause(&mut self) {
        if self.clock.is_running() {
            self.clock.pause();
        }
    }

    /// Resumes a paused recording; does nothing if it is not paused.
    pub fn resume(&mut self) {
        if self.clock.is_paused() {
            self.clock.resume();
        }
    }

    /// Pauses a running recording or resumes a paused one.
    pub fn toggle_pause(&mut self) {
        if self.is_paused() {
            self.resume()
        } else {
            self.pause()
        }
    }

    /// Drops a marker at the current recording time.
    pub fn add_marker(&mut self) {
        self.markers.push(self.clock.elapsed_secs());
    }

    /// Number of markers dropped so far.
    pub fn marker_count(&self) -> usize {
        self.markers.len()
    }

    /// Active recording time so far, excluding pauses.
    pub fn elapsed(&self) -> Duration {
        self.clock.elapsed()
    }

    /// Number of video frame slots written so far.
    #[allow(dead_code)]
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Whether the microphone is being recorded.
    pub fn has_mic(&self) -> bool {
        self.mic.is_some()
    }

    /// Whether the speakers (system audio) are being recorded.
    pub fn has_speakers(&self) -> bool {
        self.speakers.is_some()
    }

    /// Returns the error message if the video thread has ended on its own, e.g. because screen
    /// capture or encoding failed.
    ///
    /// Polled by the UI while recording. Returns `None` while the thread is running and when it
    /// ended without error; once the thread has been joined, later calls return `None`.
    pub fn video_error(&mut self) -> Option<String> {
        if self.video_thread.as_ref().is_some_and(|t| t.is_finished()) {
            let t = self.video_thread.take().unwrap();
            return match t.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(_) => Some("video capture thread panicked".into()),
            };
        }
        None
    }

    /// Stops recording, finalises all raw files and returns the session for composing.
    ///
    /// Blocks until the audio files are closed and the video thread has finished encoding.
    /// Returns the first error from an audio writer or the video pipeline.
    pub fn stop(mut self) -> Result<FinishedSession> {
        // Freeze the clock first so the recorded duration matches what was captured.
        self.clock.pause();
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(m) = self.mic.take() {
            m.finish()?;
        }
        if let Some(s) = self.speakers.take() {
            s.finish()?;
        }
        if let Some(t) = self.video_thread.take() {
            match t.join() {
                Ok(r) => r?,
                Err(_) => return Err(anyhow!("video capture thread panicked")),
            }
        }
        // Keep the markers for the editor; compose copies them next to the video.
        if !self.markers.is_empty()
            && let Ok(json) = serde_json::to_string(&self.markers)
        {
            let _ = std::fs::write(self.dir.join("markers.json"), json);
        }
        // Record the final duration and mark the session complete.
        self.manifest.duration = self.clock.elapsed_secs();
        self.manifest.finished = true;
        self.manifest.save(&self.dir)?;
        Ok(FinishedSession {
            dir: self.dir.clone(),
            manifest: self.manifest.clone(),
        })
    }

    /// Stops recording and deletes the session directory with everything recorded so far.
    pub fn cancel(self) {
        let dir = self.dir.clone();
        let _ = self.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ---------------------------------------------------------------------------
// Video: capture thread -> pacer -> encoder thread

/// Everything the video pipeline thread needs, moved into it by [`Recorder::start`].
struct VideoContext {
    /// Area to record.
    region: CaptureRegion,
    /// Settings in effect for this recording.
    settings: Settings,
    /// Path of the raw video file.
    out: PathBuf,
    /// Shared active-time clock; frames are timed against it.
    clock: ActiveClock,
    /// Set by [`Recorder::stop`] to end the pacer loop.
    stop: Arc<AtomicBool>,
    /// Frame slot counter reported back to the [`Recorder`].
    frames: Arc<AtomicU64>,
}

/// State shared between the capture thread (writer) and the pacer (reader).
#[derive(Default)]
struct SharedFrame {
    /// The newest screen image (tightly packed BGRA, region-sized) with a sequence number that
    /// increases with each new image, so the pacer can count fresh images.
    latest: Mutex<Option<(u64, Arc<Vec<u8>>)>>,
    /// A persistent capture failure, reported by the capture thread.
    error: Mutex<Option<String>>,
    /// Set while a window is recorded with window capture (following the window).
    window_mode: AtomicBool,
    /// That window's current desktop rectangle `(x, y, width, height)` in physical pixels.
    window_rect: Mutex<Option<(i32, i32, u32, u32)>>,
}

impl SharedFrame {
    /// Replaces the newest image with `bgra` and bumps the sequence number.
    fn publish(&self, bgra: Vec<u8>) {
        let mut g = self.latest.lock().unwrap();
        let seq = g.as_ref().map(|(s, _)| s + 1).unwrap_or(1);
        *g = Some((seq, Arc::new(bgra)));
    }
}

/// Capture thread body: keeps `shared` filled with the newest image of `region` until `stop` is
/// set.
///
/// Tries the capture methods from best to most compatible: window capture (when a window was
/// picked; follows the window and sees it even when covered), DXGI Desktop Duplication, and
/// finally GDI polling at `fps`. Each method falls back to the next when it fails.
/// `SOFTROMEDA_CAPTURE_METHOD=poll` skips DXGI. Errors that make capture impossible are stored in
/// `shared.error` for the pacer to report.
fn capture_thread(
    region: CaptureRegion,
    fps: f64,
    stop: Arc<AtomicBool>,
    shared: Arc<SharedFrame>,
) {
    let Some(monitor) = find_monitor(region.monitor_id) else {
        *shared.error.lock().unwrap() = Some("The selected monitor is no longer available".into());
        return;
    };
    // Desktop position of the monitor, and the region relative to it.
    let origin = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
    let rect = (region.x, region.y, region.width, region.height);
    let forced_poll = std::env::var("SOFTROMEDA_CAPTURE_METHOD").is_ok_and(|v| v == "poll");

    // A picked window: record the window itself, wherever it moves. If window capture fails
    // to start or fails mid-recording, fall through to recording the screen area instead.
    if let Some(handle) = region.window {
        match crate::media::wgc::WindowCapture::new(handle) {
            Ok(mut cap) => {
                log::info!("screen capture: window capture");
                shared.window_mode.store(true, Ordering::Relaxed);
                let (rw, rh) = (region.width, region.height);
                let mut failed = false;
                while !stop.load(Ordering::Relaxed) {
                    // Publish the window position so effects can map the mouse into the frame.
                    *shared.window_rect.lock().unwrap() = crate::media::wgc::window_bounds(handle);
                    match cap.grab() {
                        Ok(Some((px, w, h))) => {
                            // The video size is fixed: letterbox a resized window into it.
                            let px = if (w, h) == (rw, rh) {
                                px
                            } else {
                                convert::letterbox(
                                    &px,
                                    w as usize,
                                    h as usize,
                                    rw as usize,
                                    rh as usize,
                                )
                            };
                            shared.publish(px);
                        }
                        // No new image (nothing changed, or minimised): the pacer repeats the
                        // previous one.
                        Ok(None) => std::thread::sleep(Duration::from_millis(3)),
                        Err(e) => {
                            log::warn!("window capture failed ({e:#}); recording the area instead");
                            failed = true;
                            break;
                        }
                    }
                }
                shared.window_mode.store(false, Ordering::Relaxed);
                if !failed {
                    return;
                }
            }
            Err(e) => log::warn!("window capture unavailable ({e:#}); recording the area instead"),
        }
    }

    // DXGI Desktop Duplication: GPU-based, only delivers an image when the screen changed.
    if !forced_poll {
        match Duplicator::new(origin, rect) {
            Ok(mut dup) => {
                log::info!("screen capture: DXGI desktop duplication");
                let (mut grabs, mut changed) = (0u64, 0u64);
                // Wait longer for the first image; afterwards poll often enough to notice `stop`.
                let mut first = true;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        log::info!("screen capture: {changed} new images in {grabs} polls");
                        return;
                    }
                    grabs += 1;
                    match dup.grab(if first { 500 } else { 50 }) {
                        Ok(Grab::Frame(bgra)) => {
                            changed += 1;
                            first = false;
                            shared.publish(bgra);
                        }
                        Ok(Grab::Unchanged) => {
                            // Duplication only reports changes, so a static screen yields no
                            // image at all: take the first one with GDI instead.
                            if first
                                && let Ok(img) =
                                    monitor.capture_region(rect.0, rect.1, rect.2, rect.3)
                            {
                                let mut px = img.into_raw();
                                convert::swap_red_blue(&mut px);
                                shared.publish(px);
                                first = false;
                            }
                        }
                        Err(e) => {
                            log::warn!("DXGI capture failed ({e:#}); using the compatible method");
                            break;
                        }
                    }
                }
            }
            Err(e) => log::warn!("DXGI capture unavailable ({e:#}); using the compatible method"),
        }
    }

    // Compatible (GDI) capture: slower, but works everywhere. Poll at the frame rate.
    log::info!("screen capture: GDI polling");
    let interval = Duration::from_secs_f64(1.0 / fps);
    let mut errors = 0;
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        match monitor.capture_region(rect.0, rect.1, rect.2, rect.3) {
            Ok(img) => {
                errors = 0;
                let mut px = img.into_raw();
                convert::swap_red_blue(&mut px);
                shared.publish(px);
            }
            Err(e) => {
                // Tolerate occasional failures (e.g. a secure desktop); report only a long run
                // of them.
                errors += 1;
                if errors > 30 {
                    *shared.error.lock().unwrap() = Some(format!("{e}"));
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        }
        // Sleep for the rest of the frame interval.
        if let Some(rest) = interval.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Width, height and BGRA pixels of the newest webcam image, or `None` before the first one.
type CameraImage = Option<(u32, u32, Vec<u8>)>;

/// Live webcam frames for the picture-in-picture overlay, read on a background thread.
///
/// Dropping it stops the camera thread.
struct Webcam {
    /// The newest camera image.
    latest: Arc<Mutex<CameraImage>>,
    /// Tells the camera thread to finish.
    stop: Arc<AtomicBool>,
}

impl Webcam {
    /// Opens the camera named `device` (or the first camera found) on a new thread.
    ///
    /// Never fails: if the camera cannot be opened the problem is logged and the overlay simply
    /// stays empty, so the recording continues without it.
    fn start(device: Option<String>) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, st) = (latest.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("webcam".into())
            .spawn(move || {
                let mut cam = match mf::Camera::open(device.as_deref()) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("webcam: {e:#}");
                        return;
                    }
                };
                log::info!("webcam started");
                while !st.load(Ordering::Relaxed) {
                    match cam.read() {
                        Ok(Some(f)) => *l.lock().unwrap() = Some((f.width, f.height, f.bgra)),
                        Ok(None) => break,
                        Err(e) => {
                            log::warn!("webcam: {e:#}");
                            break;
                        }
                    }
                }
            });
        Self { latest, stop }
    }

    /// Draws the newest webcam image into the `w`×`h` BGRA `frame` as a rounded rectangle in the
    /// configured corner and size. Does nothing until the first camera image arrives.
    fn draw(&self, frame: &mut [u8], w: usize, h: usize, s: &Settings) {
        let Some((cw, ch, img)) = self.latest.lock().unwrap().clone() else {
            return;
        };
        // Overlay width as a percentage of the frame width, height from the camera aspect ratio.
        let tw = ((w as f64 * s.webcam_size_percent.clamp(5, 60) as f64 / 100.0) as usize).max(16);
        let th = ((tw as f64 * ch as f64 / cw.max(1) as f64) as usize).clamp(9, h);
        let scaled = convert::resize(&img, cw as usize, ch as usize, tw, th);
        // Margin from the frame edges, proportional to the frame size.
        let m = (w.min(h) / 60).max(8) as i64;
        let (x, y) = match s.webcam_position {
            WebcamPosition::TopLeft => (m, m),
            WebcamPosition::TopRight => (w as i64 - tw as i64 - m, m),
            WebcamPosition::BottomLeft => (m, h as i64 - th as i64 - m),
            WebcamPosition::BottomRight => (w as i64 - tw as i64 - m, h as i64 - th as i64 - m),
        };
        let radius = (tw.min(th) as f32 * 0.12).clamp(6.0, 48.0);
        convert::blit_rounded(frame, w, h, &scaled, tw, th, x, y, radius);
    }
}

/// Signals the camera thread to stop; it exits after its current read.
impl Drop for Webcam {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A finished frame sent from the pacer to the encoder thread.
struct EncodeJob {
    /// Tightly packed BGRA pixels at output size.
    bgra: Vec<u8>,
    /// Output width in pixels (even).
    width: usize,
    /// Output height in pixels (even).
    height: usize,
    /// Presentation time in seconds of active recording time.
    time: f64,
    /// How long the frame is shown, in seconds; longer than one frame interval when the pacer
    /// fell behind and this frame covers the missed slots.
    duration: f64,
}

/// Encoder thread body: encodes the frames received on `rx` into the H.264 file `out`.
///
/// The Media Foundation writer is created lazily from the first frame's size and is used only
/// on this thread (its COM objects must stay on one thread). Hardware encoding is tried first
/// unless `SOFTROMEDA_ENCODER=software` is set; if the hardware encoder cannot be created, or
/// fails within the first 10 seconds, encoding restarts with the software encoder. Returns when
/// the channel closes, after finalising the file; fails if no frame was ever received.
fn encoder_thread(
    rx: crossbeam_channel::Receiver<EncodeJob>,
    out: PathBuf,
    fps: f64,
    quality: u32,
) -> Result<()> {
    mf::init()?;
    let mut writer: Option<mf::Writer> = None;
    let mut hardware = std::env::var("SOFTROMEDA_ENCODER").map_or(true, |v| v != "software");
    let mut nv12 = Vec::new();
    let mut size = (0usize, 0usize);
    // Creates a writer for a `w`×`h` video, with a bitrate derived from the quality setting.
    let open = |w: usize, h: usize, hardware: bool| -> Result<mf::Writer> {
        let spec = mf::VideoSpec {
            width: w as u32,
            height: h as u32,
            fps,
            bitrate: crate::media::video_bitrate(w as u32, h as u32, fps, quality),
        };
        let mut last_err = None;
        // Prefer fragmented MP4, which stays playable after a crash; fall back to plain MP4 if
        // the system cannot write fragmented files.
        for fragmented in [true, false] {
            match mf::Writer::create(
                &out,
                mf::WriterOptions {
                    video: Some(mf::VideoInput::Encode(spec)),
                    audio: None,
                    hardware,
                    fragmented,
                },
            ) {
                Ok(w) => {
                    log::info!(
                        "encoding {}x{} H.264 at {:.1} Mbit/s ({}, {})",
                        spec.width,
                        spec.height,
                        spec.bitrate as f64 / 1e6,
                        if hardware {
                            "hardware allowed"
                        } else {
                            "software"
                        },
                        if fragmented { "fragmented MP4" } else { "MP4" }
                    );
                    return Ok(w);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap())
    };

    for job in rx {
        // Open the writer on the first frame, falling back to software if hardware fails.
        if writer.is_none() {
            size = (job.width, job.height);
            writer = Some(match open(job.width, job.height, hardware) {
                Ok(w) => w,
                Err(e) if hardware => {
                    log::warn!("hardware encoder unavailable ({e:#}); using software");
                    hardware = false;
                    open(job.width, job.height, false)?
                }
                Err(e) => return Err(e),
            });
        }
        if (job.width, job.height) != size {
            continue; // cannot happen: the pacer keeps the size fixed
        }
        convert::bgra_to_nv12(&job.bgra, job.width * 4, job.width, job.height, &mut nv12);
        let w = writer.as_ref().unwrap();
        if let Err(e) = w.write_nv12(&nv12, job.time, job.duration) {
            if !hardware || job.time > 10.0 {
                return Err(e);
            }
            // The hardware encoder accepted the format but failed on real frames. Early in the
            // recording, restart the file with the software encoder; the current frame is
            // stretched from time 0 to cover the frames already lost.
            log::warn!("hardware encoder failed ({e:#}); restarting with software encoding");
            hardware = false;
            drop(writer.take());
            let w = open(job.width, job.height, false)?;
            w.write_nv12(&nv12, 0.0, job.time + job.duration)?;
            writer = Some(w);
        }
    }
    match writer {
        Some(w) => w.finish(),
        None => Err(anyhow!("No frames were captured")),
    }
}

/// Video pipeline thread body: starts the capture and encoder threads (and the webcam), runs the
/// pacer until the recording stops, then shuts the helpers down.
///
/// An encoder error takes precedence over the pacer's result, because a failed encoder also
/// makes the pacer fail with a less useful "encoder stopped" message.
fn video_loop(ctx: VideoContext) -> Result<()> {
    let monitor = find_monitor(ctx.region.monitor_id)
        .ok_or_else(|| anyhow!("The selected monitor is no longer available"))?;
    let info =
        MonitorInfo::from_monitor(&monitor).ok_or_else(|| anyhow!("monitor info unavailable"))?;

    // Start the capture thread and a small bounded queue to the encoder; the bound applies
    // back-pressure to the pacer instead of buffering many uncompressed frames.
    let fps = ctx.settings.fps.clamp(1, 60) as f64;
    let shared = Arc::new(SharedFrame::default());
    let capture_stop = Arc::new(AtomicBool::new(false));
    let capture = {
        let (shared, stop, region) = (shared.clone(), capture_stop.clone(), ctx.region);
        std::thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || capture_thread(region, fps, stop, shared))?
    };
    let (tx, rx) = crossbeam_channel::bounded::<EncodeJob>(3);
    let encoder = {
        let (out, quality) = (ctx.out.clone(), ctx.settings.quality);
        std::thread::Builder::new()
            .name("video-encoder".into())
            .spawn(move || encoder_thread(rx, out, fps, quality))?
    };
    let webcam = ctx
        .settings
        .record_webcam
        .then(|| Webcam::start(ctx.settings.webcam_device.clone()));

    let paced = pace_frames(&ctx, &info, fps, &shared, &tx, webcam.as_ref());
    // Stop capturing and close the queue so the encoder finalises the file.
    capture_stop.store(true, Ordering::Relaxed);
    drop(tx);
    let _ = capture.join();
    let encoded = encoder
        .join()
        .map_err(|_| anyhow!("video encoder thread panicked"))?;
    // An encoder failure is the root cause of a pacer "encoder stopped" error.
    encoded?;
    paced
}

/// The pacer: produces exactly `fps` frames per second of active recording time from the newest
/// captured image until `ctx.stop` is set.
///
/// Each frame gets the drawing-on-screen layer (in window mode), mouse and keystroke effects and
/// the webcam overlay, is scaled to the output size and sent to the encoder. Nothing is produced
/// while paused. After the loop the click and mouse logs are written next to the raw video.
/// Fails if capture fails before the first image, no image arrives within 10 seconds, or the
/// encoder stops.
fn pace_frames(
    ctx: &VideoContext,
    info: &MonitorInfo,
    fps: f64,
    shared: &SharedFrame,
    tx: &crossbeam_channel::Sender<EncodeJob>,
    webcam: Option<&Webcam>,
) -> Result<()> {
    let mut effects = Effects::new(&ctx.settings, info);
    let (rw, rh) = (ctx.region.width as usize, ctx.region.height as usize);
    // Output size: scaled, and rounded down to even numbers (required by H.264 / NV12).
    let scale = ctx.settings.scale_percent.clamp(10, 100) as f64 / 100.0;
    let ow = (((rw as f64 * scale) as usize) & !1).max(2);
    let oh = (((rh as f64 * scale) as usize) & !1).max(2);

    // `written` counts frame slots (including covered repeats); `unique` counts fresh images.
    let mut written: u64 = 0;
    let mut unique: u64 = 0;
    let mut last_seq = 0;
    let wait_start = Instant::now();
    let mut have_frame = false;

    let send = |job: EncodeJob| -> Result<()> {
        tx.send(job)
            .map_err(|_| anyhow!("the video encoder stopped"))
    };

    while !ctx.stop.load(Ordering::SeqCst) {
        // A capture error is fatal only before the first image; afterwards the last image is
        // repeated so the recording is not lost.
        if let Some(e) = shared.error.lock().unwrap().take()
            && !have_frame
        {
            return Err(anyhow!("Screen capture failed: {e}"));
        }
        // Produce nothing while paused (or before the clock starts).
        if !ctx.clock.is_running() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        // Number of frame slots due by now (slot `n` starts at `n / fps` seconds, so slot 0 is
        // due at once); if they are all written, sleep until the next one. The sleep is clamped
        // so that a pause or stop request is still noticed within 20 ms.
        let due = (ctx.clock.elapsed_secs() * fps) as u64 + 1;
        if written >= due {
            let next = written as f64 / fps;
            let wait = next - ctx.clock.elapsed_secs();
            std::thread::sleep(Duration::from_secs_f64(wait.clamp(0.0005, 0.02)));
            continue;
        }

        // Take the newest image; wait (up to 10 seconds in total) for the first one.
        let snapshot = shared.latest.lock().unwrap().clone();
        let Some((seq, image)) = snapshot else {
            if wait_start.elapsed() > Duration::from_secs(10) {
                return Err(anyhow!("No screen image could be captured"));
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        have_frame = true;
        if seq != last_seq {
            unique += 1;
            last_seq = seq;
        }

        // Draw effects on a copy at full region resolution. Skip an image of the wrong size.
        let mut data = image.as_ref().clone();
        drop(image);
        if data.len() != rw * rh * 4 {
            continue;
        }
        if let Some(mut pixmap) =
            IntSize::from_wh(rw as u32, rh as u32).and_then(|s| Pixmap::from_vec(data, s))
        {
            // The followed window's desktop rectangle, when window capture is active.
            let window = shared
                .window_mode
                .load(Ordering::Relaxed)
                .then(|| *shared.window_rect.lock().unwrap())
                .flatten();
            if window.is_some() {
                // Window capture does not see the on-screen drawing window, so blend the
                // drawings into the frame here.
                crate::overlay::blend_layer(pixmap.data_mut(), rw as u32, rh as u32);
            }
            effects.apply(
                &mut pixmap,
                &ctx.region,
                info,
                ctx.clock.elapsed_secs(),
                window,
            );
            data = pixmap.take();
        } else {
            return Err(anyhow!("invalid frame size {rw}x{rh}"));
        }
        if let Some(cam) = webcam {
            cam.draw(&mut data, rw, rh, &ctx.settings);
        }
        // Scale to the output size; at 100% only an odd row or column needs dropping.
        let data = if (ow, oh) == (rw, rh) {
            data
        } else if ow <= rw && oh <= rh && scale >= 0.999 {
            convert::crop(&data, rw, 0, 0, ow, oh) // just drop an odd row/column
        } else {
            convert::resize(&data, rw, rh, ow, oh)
        };

        // Slots that could not be produced in time are covered by giving this frame a longer
        // duration instead of encoding duplicates.
        let due_now = (ctx.clock.elapsed_secs() * fps) as u64 + 1;
        let repeat = due_now.saturating_sub(written).max(1);
        send(EncodeJob {
            bgra: data,
            width: ow,
            height: oh,
            time: written as f64 / fps,
            duration: repeat as f64 / fps,
        })?;
        written += repeat;
        ctx.frames.store(written, Ordering::Relaxed);
    }

    // Save clicks (for automatic zoom) and mouse positions (for vertical export) in the session
    // directory; compose copies them next to the final video.
    for (log, name) in [
        (&effects.click_log, "clicks.json"),
        (&effects.mouse_log, "mouse.json"),
    ] {
        if !log.is_empty()
            && let Ok(json) = serde_json::to_string(log)
        {
            let _ = std::fs::write(ctx.out.with_file_name(name), json);
        }
    }
    log::info!(
        "video: {written} frame slots at {fps} fps, {unique} new screen images ({:.0}% fresh)",
        unique as f64 * 100.0 / written.max(1) as f64
    );
    if written == 0 {
        return Err(anyhow!("No frames were captured"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor, click and keystroke effects

/// Index of the right mouse button in `device_query`'s `button_pressed` list (1 is left).
const RIGHT_BUTTON: usize = 2;

/// A mouse or keyboard event detected by the [`InputMonitor`].
enum InputEvent {
    /// A mouse button was pressed at desktop position (`x`, `y`) in physical pixels.
    Click {
        /// Desktop x coordinate of the pointer, in physical pixels.
        x: i32,
        /// Desktop y coordinate of the pointer, in physical pixels.
        y: i32,
        /// Right button (otherwise left).
        right: bool,
        /// When the press was detected.
        at: Instant,
    },
    /// A key was pressed.
    Key {
        /// Text to show, e.g. `"a"`, `"Enter"` or `"Ctrl + Shift + S"`.
        label: String,
        /// Typed text (a printable character or space without shortcut modifiers) that may be
        /// appended to the previous caption; otherwise the caption is replaced.
        plain: bool,
    },
}

/// Input state shared between the monitor thread and the pacer.
#[derive(Default)]
struct InputShared {
    /// Current pointer position on the desktop, in physical pixels.
    mouse: (i32, i32),
    /// Whether any mouse button is held down.
    any_button: bool,
    /// Events not yet consumed by the pacer.
    events: Vec<InputEvent>,
}

/// Samples mouse and keyboard at about 200 Hz on its own thread so that short clicks and fast
/// typing are not missed between video frames.
///
/// Dropping it stops the thread.
struct InputMonitor {
    /// State filled by the thread and drained by [`Effects::apply`].
    shared: Arc<std::sync::Mutex<InputShared>>,
    /// Tells the thread to finish.
    stop: Arc<AtomicBool>,
}

impl InputMonitor {
    /// Starts the sampling thread; keyboard events are generated only when `want_keys` is set.
    ///
    /// Returns `None` (and logs a warning) if the input state cannot be read within 2 seconds.
    fn start(want_keys: bool) -> Option<Self> {
        let shared = Arc::new(std::sync::Mutex::new(InputShared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<bool>(1);
        let (sh, st) = (shared.clone(), stop.clone());
        std::thread::Builder::new()
            .name("input-monitor".into())
            .spawn(move || {
                // Report to the caller whether input can be read at all.
                let Some(device) = DeviceState::checked_new() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                let mut prev_buttons = (false, false);
                let mut prev_keys: Vec<Keycode> = vec![];
                while !st.load(Ordering::Relaxed) {
                    let mouse = device.get_mouse();
                    let left = mouse.button_pressed.get(1).copied().unwrap_or(false);
                    let right = mouse
                        .button_pressed
                        .get(RIGHT_BUTTON)
                        .copied()
                        .unwrap_or(false);
                    let mut events = vec![];
                    let now = Instant::now();
                    let (x, y) = mouse.coords;
                    // A click is a transition from released to pressed.
                    if left && !prev_buttons.0 {
                        events.push(InputEvent::Click {
                            x,
                            y,
                            right: false,
                            at: now,
                        });
                    }
                    if right && !prev_buttons.1 {
                        events.push(InputEvent::Click {
                            x,
                            y,
                            right: true,
                            at: now,
                        });
                    }
                    prev_buttons = (left, right);
                    // Newly pressed non-modifier keys become captions: printable characters
                    // (optionally with Shift) and Space are typed text; anything else, or a
                    // key with Ctrl/Alt/Win, is shown as a shortcut such as "Ctrl + C".
                    if want_keys {
                        let keys = device.get_keys();
                        for k in keys
                            .iter()
                            .filter(|k| !prev_keys.contains(k) && !is_modifier(k))
                        {
                            let mods = modifiers_label(&keys);
                            let shift = keys
                                .iter()
                                .any(|k| matches!(k, Keycode::LShift | Keycode::RShift));
                            let label = key_label(k);
                            if *k == Keycode::Space && mods.is_empty() {
                                events.push(InputEvent::Key {
                                    label: " ".into(),
                                    plain: true,
                                });
                            } else if label.chars().count() == 1
                                && (mods.is_empty() || mods == "Shift")
                            {
                                let label = if shift { label } else { label.to_lowercase() };
                                events.push(InputEvent::Key { label, plain: true });
                            } else if mods.is_empty() {
                                events.push(InputEvent::Key {
                                    label,
                                    plain: false,
                                });
                            } else {
                                events.push(InputEvent::Key {
                                    label: format!("{mods} + {label}"),
                                    plain: false,
                                });
                            }
                        }
                        prev_keys = keys;
                    }
                    // Publish the state; cap the queue in case the pacer is not draining it
                    // (e.g. while paused) by dropping the oldest half.
                    {
                        let mut g = sh.lock().unwrap();
                        g.mouse = (x, y);
                        g.any_button = mouse.button_pressed.iter().any(|b| *b);
                        g.events.extend(events);
                        if g.events.len() > 256 {
                            g.events.drain(..128);
                        }
                    }
                    // 5 ms between samples: about 200 Hz, as documented on the type.
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .ok()?;
        // Wait for the thread to report whether input is readable.
        if ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false)
        {
            Some(Self { shared, stop })
        } else {
            log::warn!("mouse/keyboard state unavailable (missing permission?)");
            None
        }
    }
}

/// Signals the sampling thread to stop; it exits within one sampling interval.
impl Drop for InputMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A click ripple being animated.
struct ClickFx {
    /// Horizontal centre in frame pixels.
    x: i32,
    /// Vertical centre in frame pixels.
    y: i32,
    /// Right button (selects the ripple colour).
    right: bool,
    /// When the click happened; the ripple's age drives its size and fade.
    at: Instant,
}

/// Draws the mouse and keyboard effects into each frame and logs clicks and mouse positions.
///
/// Colours are stored in BGRA order to match the frame pixels.
struct Effects {
    /// Input sampler, or `None` if input cannot be read (then no effects are drawn).
    input: Option<InputMonitor>,
    /// Font for the keystroke caption.
    font: FontArc,
    /// Draw the mouse pointer.
    pub show_cursor: bool,
    /// Pointer highlight colour and radius (in captured pixels at 100% DPI), if enabled.
    highlight: Option<([u8; 4], f32)>,
    /// Spotlight radius (captured pixels at 100 % scaling) and dimming, if enabled.
    spotlight: Option<(f32, f32)>,
    /// Smooth the drawn pointer's movement.
    smooth: bool,
    /// Smoothed pointer position (frame pixels) and when it was last updated.
    smoothed: Option<((f32, f32), Instant)>,
    /// Draw click ripples.
    show_clicks: bool,
    /// Left-click ripple colour.
    left_color: [u8; 4],
    /// Right-click ripple colour.
    right_color: [u8; 4],
    /// Draw the keystroke caption.
    show_keys: bool,
    /// Click ripples still being animated.
    clicks: VecDeque<ClickFx>,
    /// Every left click (for automatic zoom in the editor).
    pub click_log: Vec<crate::editor::project::Click>,
    /// Mouse position up to 10 times per second when it moves (for vertical export).
    pub mouse_log: Vec<crate::editor::project::Click>,
    /// Current keystroke caption.
    key_text: String,
    /// Whether the caption is typed text that the next typed key may extend.
    key_plain: bool,
    /// When the caption last changed; it fades out 1.5–2 seconds later.
    key_at: Instant,
    /// Monitor DPI scale (at least 1), so effects keep their apparent size on high-DPI screens.
    scale: f32,
}

impl Effects {
    /// Builds the effects from the settings and starts the input monitor.
    fn new(s: &Settings, info: &MonitorInfo) -> Self {
        Self {
            // Always watch the mouse: clicks and positions are logged for
            // automatic zoom and vertical export.
            input: InputMonitor::start(s.show_keystrokes),
            font: draw::load_font(s.font_bytes()),
            show_cursor: s.show_cursor,
            // Frames are BGRA, so configured colours need red/blue swapped.
            highlight: s
                .highlight_cursor
                .then_some((bgra(s.highlight_color), s.highlight_radius)),
            spotlight: s.spotlight.then_some((s.spotlight_radius, s.spotlight_dim)),
            smooth: s.smooth_cursor,
            smoothed: None,
            show_clicks: s.show_clicks,
            left_color: bgra(s.left_click_color),
            right_color: bgra(s.right_click_color),
            show_keys: s.show_keystrokes,
            clicks: VecDeque::new(),
            click_log: vec![],
            mouse_log: vec![],
            key_text: String::new(),
            key_plain: false,
            // Long ago, so no caption is shown at the start.
            key_at: Instant::now() - Duration::from_secs(60),
            scale: info.scale_factor.max(1.0),
        }
    }

    /// Consumes pending input events, updates the logs and draws the effects into `pm`.
    ///
    /// `now` is the active recording time in seconds (pauses excluded), used to time-stamp the
    /// logs. `window` is the followed window's desktop rectangle in window-capture mode, in which
    /// case mouse positions are mapped relative to it; otherwise they are mapped relative to
    /// `region` on monitor `info`. Returns `false` when input is unavailable and nothing was drawn.
    fn apply(
        &mut self,
        pm: &mut Pixmap,
        region: &CaptureRegion,
        info: &MonitorInfo,
        now: f64,
        window: Option<(i32, i32, u32, u32)>,
    ) -> bool {
        let Some(input) = &self.input else {
            return false;
        };
        let (mouse, any_button, events) = {
            let mut g = input.shared.lock().unwrap();
            (g.mouse, g.any_button, std::mem::take(&mut g.events))
        };
        // Mapping from desktop to frame pixels: scale (`ppu`), the desktop position of the
        // captured area (`ox`, `oy`) and the frame offset (`dx`, `dy`). A followed window that
        // was resized is letterboxed into the frame, which these values mirror.
        let (ppu, ox, oy, dx, dy) = match window {
            Some((wx, wy, ww, wh)) => {
                let k = (pm.width() as f32 / ww.max(1) as f32)
                    .min(pm.height() as f32 / wh.max(1) as f32);
                let dx = (pm.width() as f32 - ww as f32 * k) / 2.0;
                let dy = (pm.height() as f32 - wh as f32 * k) / 2.0;
                (k, wx, wy, dx, dy)
            }
            None => (
                pm.width() as f32 / region.width.max(1) as f32,
                info.x + region.x as i32,
                info.y + region.y as i32,
                0.0,
                0.0,
            ),
        };
        let to_frame =
            |(x, y): (i32, i32)| ((x - ox) as f32 * ppu + dx, (y - oy) as f32 * ppu + dy);
        let (fx, fy) = to_frame(mouse);
        let px_scale = self.scale * ppu;

        // Log the mouse position as a fraction of the frame size, at most 10 times a second and
        // only when it moved noticeably and is inside the frame.
        {
            let (u, v) = (fx / pm.width() as f32, fy / pm.height() as f32);
            let last = self.mouse_log.last().copied();
            let due = last.is_none_or(|l| now - l.t >= 0.1);
            let moved = last.is_none_or(|l| (l.x - u).abs() + (l.y - v).abs() > 0.002);
            if due && moved && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                self.mouse_log
                    .push(crate::editor::project::Click { t: now, x: u, y: v });
            }
        }
        let drawing = crate::overlay::DRAWING.load(std::sync::atomic::Ordering::Relaxed);
        for ev in &events {
            if let InputEvent::Click {
                x,
                y,
                right: false,
                at,
            } = ev
            {
                // Log left clicks inside the frame, back-dated to when they happened. Skip
                // clicks while drawing on screen (pen strokes) and stale ones made while paused.
                let age = at.elapsed().as_secs_f64();
                let (cx, cy) = to_frame((*x, *y));
                let (u, v) = (cx / pm.width() as f32, cy / pm.height() as f32);
                if !drawing && age < 1.0 && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                    self.click_log.push(crate::editor::project::Click {
                        t: (now - age).max(0.0),
                        x: u,
                        y: v,
                    });
                }
            }
        }
        // Turn events into ripples and keystroke captions.
        for ev in events {
            match ev {
                // Clicks while drawing on the screen are pen strokes.
                InputEvent::Click { x, y, right, at }
                    if self.show_clicks
                        && !crate::overlay::DRAWING.load(std::sync::atomic::Ordering::Relaxed) =>
                {
                    let (cx, cy) = to_frame((x, y));
                    self.clicks.push_back(ClickFx {
                        x: cx as i32,
                        y: cy as i32,
                        right,
                        at,
                    });
                }
                // Consecutive typed characters within 1.5 s build up a word (up to 40
                // characters); anything else replaces the caption.
                InputEvent::Key { label, plain } if self.show_keys => {
                    let recent = self.key_at.elapsed() < Duration::from_millis(1500);
                    if plain && self.key_plain && recent && self.key_text.chars().count() < 40 {
                        self.key_text.push_str(&label);
                    } else {
                        self.key_text = label;
                    }
                    self.key_plain = plain;
                    self.key_at = Instant::now();
                }
                _ => {}
            }
        }

        // Where the pointer, its highlight and the spotlight are drawn: the real position, or a
        // smoothed one that follows it with a short (70 ms) lag so shaky movement glides. Big
        // jumps (a quarter of the frame or more) are not smoothed. Ripples and the logs above
        // keep the exact position.
        let (fx, fy) = if self.smooth {
            let now_i = Instant::now();
            let p = match self.smoothed {
                Some(((x, y), at)) if (fx - x).hypot(fy - y) < pm.width() as f32 * 0.25 => {
                    // Exponential smoothing with a 70 ms time constant, independent of the
                    // frame rate because the factor is derived from the elapsed time.
                    let dt = now_i.duration_since(at).as_secs_f32();
                    let k = 1.0 - (-dt / 0.07).exp();
                    (x + (fx - x) * k, y + (fy - y) * k)
                }
                _ => (fx, fy),
            };
            self.smoothed = Some((p, now_i));
            p
        } else {
            (fx, fy)
        };

        // Spotlight: darken everything except a circle around the pointer (drawn first, so the
        // highlight, ripples and caption stay bright). Skipped while the pointer is far outside
        // the frame, where the whole picture would go dark.
        if let Some((r, dim)) = self.spotlight {
            let radius = r * px_scale;
            let (w, h) = (pm.width() as f32, pm.height() as f32);
            let near = fx > -radius && fy > -radius && fx < w + radius && fy < h + radius;
            if near {
                let (pw, ph) = (pm.width() as usize, pm.height() as usize);
                convert::spotlight(pm.data_mut(), pw, ph, fx, fy, radius, dim);
            }
        }

        // Highlight under the pointer: slightly smaller and more opaque while a button is held.
        if let Some((col, r)) = self.highlight {
            if any_button {
                draw::fill_circle(
                    pm,
                    fx,
                    fy,
                    r * px_scale * 0.85,
                    [col[0], col[1], col[2], col[3].saturating_add(40)],
                );
            } else {
                draw::fill_circle(pm, fx, fy, r * px_scale, col);
            }
        }

        // Click ripples: a ring that grows and fades out over 0.45 s.
        let life = 0.45f32;
        self.clicks.retain(|c| c.at.elapsed().as_secs_f32() < life);
        for c in &self.clicks {
            let t = c.at.elapsed().as_secs_f32() / life;
            let col = if c.right {
                self.right_color
            } else {
                self.left_color
            };
            let alpha = (col[3] as f32 * (1.0 - t)) as u8;
            let r = (8.0 + 26.0 * t) * px_scale;
            draw::stroke_circle(
                pm,
                c.x as f32,
                c.y as f32,
                r,
                3.0 * px_scale,
                [col[0], col[1], col[2], alpha],
            );
        }

        if self.show_cursor {
            draw::draw_cursor(pm, fx, fy, px_scale);
        }

        // Keystroke caption centred near the bottom; it fades out between 1.5 and 2 s.
        if self.show_keys && !self.key_text.is_empty() {
            let age = self.key_at.elapsed().as_secs_f32();
            if age < 2.0 {
                let alpha = if age > 1.5 {
                    ((2.0 - age) / 0.5 * 255.0) as u8
                } else {
                    255
                };
                let size = (pm.height() as f32 * 0.045).clamp(16.0, 64.0);
                let bottom = pm.height() as f32 - size * 1.2;
                draw::draw_label(
                    pm,
                    &self.font,
                    size,
                    pm.width() as f32 / 2.0,
                    bottom,
                    &self.key_text,
                    [255, 255, 255, alpha],
                    [20, 20, 20, (alpha as f32 * 0.75) as u8],
                );
            }
        }
        true
    }
}

/// Converts an RGBA colour to BGRA byte order (swaps red and blue).
fn bgra(c: [u8; 4]) -> [u8; 4] {
    [c[2], c[1], c[0], c[3]]
}

/// Whether `k` is a modifier key (Ctrl, Shift, Alt or Windows/Command), which is never shown on
/// its own.
fn is_modifier(k: &Keycode) -> bool {
    matches!(
        k,
        Keycode::LControl
            | Keycode::RControl
            | Keycode::LShift
            | Keycode::RShift
            | Keycode::LAlt
            | Keycode::RAlt
            | Keycode::LMeta
            | Keycode::RMeta
            | Keycode::Command
            | Keycode::RCommand
            | Keycode::LOption
            | Keycode::ROption
    )
}

/// The held modifiers among `keys` in a fixed order, e.g. `"Ctrl + Shift"`; empty if none.
fn modifiers_label(keys: &[Keycode]) -> String {
    let has = |a: &[Keycode]| keys.iter().any(|k| a.contains(k));
    let mut parts = vec![];
    if has(&[Keycode::LControl, Keycode::RControl]) {
        parts.push("Ctrl");
    }
    if has(&[
        Keycode::LAlt,
        Keycode::RAlt,
        Keycode::LOption,
        Keycode::ROption,
    ]) {
        parts.push("Alt");
    }
    if has(&[Keycode::LShift, Keycode::RShift]) {
        parts.push("Shift");
    }
    if has(&[
        Keycode::LMeta,
        Keycode::RMeta,
        Keycode::Command,
        Keycode::RCommand,
    ]) {
        parts.push("Win");
    }
    parts.join(" + ")
}

/// Display text for key `k`: its name without a `Key` prefix, with short names or arrow symbols
/// for a few special keys.
fn key_label(k: &Keycode) -> String {
    let s = format!("{k}");
    let s = s.strip_prefix("Key").unwrap_or(&s).to_string();
    match s.as_str() {
        "Space" => "Space".into(),
        "Enter" => "Enter".into(),
        "Backspace" => "Backspace".into(),
        "Escape" => "Esc".into(),
        "Up" => "↑".into(),
        "Down" => "↓".into(),
        "Left" => "←".into(),
        "Right" => "→".into(),
        _ => s,
    }
}
