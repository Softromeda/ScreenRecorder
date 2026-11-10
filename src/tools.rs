// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Conversion tools: Convert to MP4, Convert to GIF, Extract Audio and Join Videos.
//!
//! [`ToolsWindow`] is a small secondary window, opened from the main window's Tools menu, where
//! the user picks input files and options for one [`ToolKind`]. The work runs in a background
//! [`Job`] through the editor's render engine ([`render::render`]) or, for audio extraction,
//! its audio decoder plus the audio-file writer. The free functions [`join`], [`convert_gif`]
//! and [`extract_audio`] contain the actual conversions and can be called on any thread.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::editor::export::even;
use crate::editor::render::{self, Output, Part, RenderJob};
#[allow(unused_imports)]
use crate::i18n::{tf, tr};
use crate::job::{Job, JobStatus};
use crate::media::mf;

/// The conversion a [`ToolsWindow`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Re-encode one video as H.264/AAC MP4, optionally at a lower resolution.
    ConvertMp4,
    /// Turn one video into an animated GIF.
    ConvertGif,
    /// Save the audio track of one video as M4A (AAC) or MP3.
    ExtractAudio,
    /// Concatenate two or more videos into one MP4.
    Join,
}

/// Output resolution choice for MP4 conversion and joining, named by frame height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Keep the source size.
    Original,
    /// 2160 pixels high (4K UHD).
    P2160,
    /// 1440 pixels high (QHD).
    P1440,
    /// 1080 pixels high (Full HD).
    P1080,
    /// 720 pixels high (HD).
    P720,
    /// 480 pixels high.
    P480,
    /// 360 pixels high.
    P360,
}

impl Resolution {
    /// Every choice in the order shown in the drop-down list.
    const ALL: [Resolution; 7] = [
        Resolution::Original,
        Resolution::P2160,
        Resolution::P1440,
        Resolution::P1080,
        Resolution::P720,
        Resolution::P480,
        Resolution::P360,
    ];

    /// Returns the label shown in the drop-down list (only Original size is translated).
    fn label(self) -> &'static str {
        match self {
            Resolution::Original => tr("Original size"),
            Resolution::P2160 => "2160p (4K)",
            Resolution::P1440 => "1440p",
            Resolution::P1080 => "1080p",
            Resolution::P720 => "720p",
            Resolution::P480 => "480p",
            Resolution::P360 => "360p",
        }
    }

    /// Returns the target frame height in pixels, or `None` to keep the source size.
    fn height(self) -> Option<u32> {
        match self {
            Resolution::Original => None,
            Resolution::P2160 => Some(2160),
            Resolution::P1440 => Some(1440),
            Resolution::P1080 => Some(1080),
            Resolution::P720 => Some(720),
            Resolution::P480 => Some(480),
            Resolution::P360 => Some(360),
        }
    }
}

/// State of the conversion-tools window: the selected tool, its inputs and options, and the
/// running or finished job.
pub struct ToolsWindow {
    /// Whether the window is shown.
    pub open: bool,
    /// The tool the window is set up for.
    pub kind: ToolKind,
    /// Input files; exactly one for the single-file tools, two or more (in order) for joining.
    files: Vec<PathBuf>,
    /// Output resolution for MP4 conversion and joining.
    resolution: Resolution,
    /// MP4 quality, 1 to 100.
    quality: u32,
    /// GIF frame rate in frames per second.
    gif_fps: u32,
    /// Maximum GIF width in pixels; wider videos are scaled down.
    gif_width: u32,
    /// Extract audio as MP3 (`true`) or M4A/AAC (`false`).
    audio_mp3: bool,
    /// The current or last conversion; its status is shown in the window.
    job: Option<Job>,
}

/// A closed window set up for MP4 conversion with moderate defaults.
impl Default for ToolsWindow {
    fn default() -> Self {
        Self {
            open: false,
            kind: ToolKind::ConvertMp4,
            files: vec![],
            resolution: Resolution::Original,
            quality: 70,
            gif_fps: 12,
            gif_width: 800,
            audio_mp3: false,
            job: None,
        }
    }
}

impl ToolsWindow {
    /// Opens the window for `kind`; the file list is cleared when switching to another tool.
    pub fn open_tool(&mut self, kind: ToolKind) {
        self.open = true;
        if self.kind != kind {
            self.files.clear();
        }
        self.kind = kind;
    }

    /// Shows the window as a separate native viewport while it is open; call every frame.
    ///
    /// Handles closing, and keeps repainting every 100 ms while a job runs so that its
    /// progress bar updates.
    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let title = match self.kind {
            ToolKind::ConvertMp4 => tr("Convert to MP4"),
            ToolKind::ConvertGif => tr("Convert to GIF"),
            ToolKind::ExtractAudio => tr("Extract Audio"),
            ToolKind::Join => tr("Join Videos"),
        };
        let mut close = false;
        // The viewport starts hidden and is revealed by `reveal_when_ready` once laid out.
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("tools"),
            crate::geometry::centered(
                egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size([480.0, 380.0])
                    .with_min_inner_size([420.0, 300.0])
                    .with_max_inner_size([900.0, 800.0])
                    .with_maximize_button(false)
                    .with_visible(false),
                [480.0, 380.0],
            ),
            |ui, _| {
                crate::util::reveal_when_ready(ui.ctx());
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading(title);
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| self.contents(ui));
                });
            },
        );
        if close {
            self.open = false;
            crate::util::forget_reveal(ctx, egui::ViewportId::from_hash_of("tools"));
        }
        if self.job.as_ref().is_some_and(|j| !j.is_finished()) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Draws the window contents: file list, tool options, the Convert button and job status.
    fn contents(&mut self, ui: &mut egui::Ui) {
        let multi = self.kind == ToolKind::Join;
        // File picker: joining adds to the list, the other tools replace the single file.
        ui.horizontal(|ui| {
            if ui
                .button(if multi {
                    tr("➕ Add videos…")
                } else {
                    tr("📂 Choose file…")
                })
                .clicked()
            {
                let dlg = rfd::FileDialog::new()
                    .add_filter("Video", &["mp4", "mov", "m4v", "avi", "wmv", "mkv"]);
                if multi {
                    if let Some(files) = dlg.pick_files() {
                        self.files.extend(files);
                    }
                } else if let Some(f) = dlg.pick_file() {
                    self.files = vec![f];
                }
            }
            if !self.files.is_empty() && ui.button(tr("Clear")).clicked() {
                self.files.clear();
            }
        });
        // File list; for joining, each entry can be moved up or removed. Changes are applied
        // after the loop because the list is borrowed while drawing.
        let mut remove = None;
        let mut swap = None;
        for (i, f) in self.files.iter().enumerate() {
            ui.horizontal(|ui| {
                if multi {
                    if ui.small_button("⬆").clicked() && i > 0 {
                        swap = Some((i, i - 1));
                    }
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                }
                ui.label(
                    f.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                );
            });
        }
        if let Some((a, b)) = swap {
            self.files.swap(a, b);
        }
        if let Some(i) = remove {
            self.files.remove(i);
        }
        ui.separator();

        // Options for the selected tool.
        match self.kind {
            ToolKind::ConvertMp4 | ToolKind::Join => {
                egui::ComboBox::from_label(tr("Resolution"))
                    .selected_text(self.resolution.label())
                    .show_ui(ui, |ui| {
                        for r in Resolution::ALL {
                            ui.selectable_value(&mut self.resolution, r, r.label());
                        }
                    });
                ui.add(egui::Slider::new(&mut self.quality, 1..=100).text(tr("Quality")));
            }
            ToolKind::ConvertGif => {
                ui.add(egui::Slider::new(&mut self.gif_fps, 5..=30).text(tr("Frames per second")));
                ui.add(egui::Slider::new(&mut self.gif_width, 160..=1920).text(tr("Max width")));
            }
            ToolKind::ExtractAudio => {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.audio_mp3, false, tr("M4A (AAC)"));
                    ui.radio_value(&mut self.audio_mp3, true, tr("MP3"));
                });
            }
        }

        // Convert is enabled when the right number of files is chosen and no job is running.
        let busy = self.job.as_ref().is_some_and(|j| !j.is_finished());
        let ready = if multi {
            self.files.len() >= 2
        } else {
            self.files.len() == 1
        };
        ui.add_space(6.0);
        ui.add_enabled_ui(ready && !busy, |ui| {
            if ui.button(tr("▶ Convert…")).clicked() {
                self.start();
            }
        });
        // Status of the current or last job: progress, result with shortcuts, or error.
        if let Some(job) = &self.job {
            match job.status() {
                JobStatus::Running { progress, message } => {
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .text(message)
                            .show_percentage(),
                    );
                }
                JobStatus::Done { output, .. } => {
                    ui.colored_label(
                        crate::util::ok_color(ui),
                        tf("Saved: {}", &[&output.display()]),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(tr("Open")).clicked() {
                            crate::util::open_path(&output);
                        }
                        if ui.button(tr("Show in folder")).clicked() {
                            crate::util::reveal_in_folder(&output);
                        }
                    });
                }
                JobStatus::Failed(e) => {
                    ui.colored_label(crate::util::error_color(ui), e);
                }
            }
        }
    }

    /// Asks for the output file and starts the conversion as a background job.
    ///
    /// The suggested name is derived from the first input (with a suffix such as `_converted`
    /// or `_joined` where the extension alone would not distinguish it) in the input's folder.
    /// Does nothing if the user cancels the save dialog.
    fn start(&mut self) {
        let first = self.files[0].clone();
        let stem = first
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "output".into());
        let (suffix, ext) = match self.kind {
            ToolKind::ConvertMp4 => ("_converted", "mp4"),
            ToolKind::ConvertGif => ("", "gif"),
            ToolKind::ExtractAudio => ("", if self.audio_mp3 { "mp3" } else { "m4a" }),
            ToolKind::Join => ("_joined", "mp4"),
        };
        let Some(out) = rfd::FileDialog::new()
            .set_directory(first.parent().unwrap_or(Path::new(".")))
            .set_file_name(format!("{stem}{suffix}.{ext}"))
            .save_file()
        else {
            return;
        };
        // Add the expected extension if the user typed a name without one.
        let out = if out.extension().is_none() {
            out.with_extension(ext)
        } else {
            out
        };
        // Writing over an input while reading it would destroy it; report the problem through
        // a failed placeholder job so it appears where job errors normally do.
        if self.files.contains(&out) {
            // The placeholder job never runs, so its name is irrelevant.
            self.job = Some(Job::new("x"));
            self.job.as_ref().unwrap().set(JobStatus::Failed(
                tr("Output must differ from the input file.").into(),
            ));
            return;
        }
        // Copy the settings into the job closure, which runs on another thread.
        let files = self.files.clone();
        let kind = self.kind;
        let res = self.resolution;
        let quality = self.quality;
        let (gif_fps, gif_width, mp3) = (self.gif_fps, self.gif_width, self.audio_mp3);
        self.job = Some(Job::spawn("Convert", move |job| {
            match kind {
                // Converting is joining a single file.
                ToolKind::ConvertMp4 => join(&files[..1], &out, res, quality, job)?,
                ToolKind::ConvertGif => convert_gif(&files[0], &out, gif_fps, gif_width, job)?,
                ToolKind::ExtractAudio => extract_audio(&files[0], &out, mp3, job)?,
                ToolKind::Join => join(&files, &out, res, quality, job)?,
            }
            Ok((out, tr("Done").into()))
        }));
    }
}

/// Returns the output frame size for a `w` x `h` source at resolution `res`.
///
/// The aspect ratio is kept and both sides are rounded to even numbers for H.264. The video is
/// never upscaled: a target height at or above the source height keeps the source size.
fn target_size(w: u32, h: u32, res: Resolution) -> (u32, u32) {
    match res.height() {
        Some(th) if th < h => (
            even((w as f64 * th as f64 / h as f64).round() as u32),
            even(th),
        ),
        _ => (even(w), even(h)),
    }
}

/// Builds a [`RenderJob`] that plays `parts` of `sources` unchanged: no overlays, normal
/// speed and volume, no fades, zoom-follow or background music.
fn simple_job(
    sources: Vec<PathBuf>,
    parts: Vec<Part>,
    w: u32,
    h: u32,
    fps: f64,
    output: Output,
) -> RenderJob {
    RenderJob {
        sources,
        parts,
        width: w,
        height: h,
        fps,
        overlays: vec![],
        speed: 1.0,
        volume: 1.0,
        mute: false,
        fade_in: 0.0,
        fade_out: 0.0,
        output,
        follow: None,
        music: None,
        badges: vec![],
        logo: None,
    }
}

/// Converts one file, or joins several files end to end, into an H.264/AAC MP4 at `out`.
///
/// The output size and frame rate (limited to 1-60 fps) come from the first input that has
/// video, scaled by `res`; clips of a different size are letterboxed into it. `quality` is
/// 1 to 100. Progress is reported through `job`. Fails if an input cannot be read or none has
/// video.
pub fn join(
    inputs: &[PathBuf],
    out: &Path,
    res: Resolution,
    quality: u32,
    job: &Job,
) -> Result<()> {
    let infos = inputs
        .iter()
        .map(|p| mf::probe(p))
        .collect::<Result<Vec<_>>>()?;
    let first = infos
        .iter()
        .find(|i| i.has_video)
        .ok_or_else(|| anyhow!("No video input"))?;
    let (w, h) = target_size(first.width, first.height, res);
    // One part per input covering its whole duration, in list order.
    let parts = infos
        .iter()
        .enumerate()
        .map(|(i, info)| Part::Clip {
            source: i,
            start: 0.0,
            end: info.duration,
            zoom: None,
            speed: 1.0,
        })
        .collect();
    let spec = simple_job(
        inputs.to_vec(),
        parts,
        w,
        h,
        first.fps.clamp(1.0, 60.0),
        Output::Mp4 { quality },
    );
    render::render(&spec, out, job)
}

/// Converts a whole video into an animated GIF at `out`.
///
/// `fps` is the GIF frame rate (limited to 1-50) and `width` the maximum width in pixels (at
/// least 64); wider videos are scaled down. Fails if the input has no video.
pub fn convert_gif(input: &Path, out: &Path, fps: u32, width: u32, job: &Job) -> Result<()> {
    let info = mf::probe(input)?;
    if !info.has_video {
        return Err(anyhow!("The input has no video"));
    }
    let parts = vec![Part::Clip {
        source: 0,
        start: 0.0,
        end: info.duration,
        zoom: None,
        speed: 1.0,
    }];
    // The job is given the source size; the renderer scales it down to `max_width`.
    let spec = simple_job(
        vec![input.to_path_buf()],
        parts,
        even(info.width),
        even(info.height),
        fps as f64,
        Output::Gif {
            fps: fps.clamp(1, 50) as f64,
            max_width: width.max(64),
        },
    );
    render::render(&spec, out, job)
}

/// Saves the audio track of `input` as MP3 (`mp3 == true`) or AAC in an M4A file at `out`.
///
/// The audio is decoded to 48 kHz stereo and re-encoded. Fails if the input has no audio.
pub fn extract_audio(input: &Path, out: &Path, mp3: bool, job: &Job) -> Result<()> {
    let samples = render::decode_audio(input, job)?;
    if samples.is_empty() {
        return Err(anyhow!("The input has no audio"));
    }
    let codec = if mp3 {
        mf::AudioCodec::Mp3
    } else {
        mf::AudioCodec::Aac
    };
    job.set_progress(0.85, tr("Encoding audio…"));
    crate::compose::write_audio_file(out, codec, &samples, None)
}
