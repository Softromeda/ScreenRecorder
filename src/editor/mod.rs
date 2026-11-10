// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Built-in video editor.
//!
//! The editor opens a recording in its own native window and offers trim, cuts, zoom-and-pan
//! effects, annotations, title pages, pause removal, speed, volume, fades, background music and
//! export to MP4 or GIF in the original, vertical or square shape. Submodules: [`project`] holds
//! the edit model saved next to the video, [`player`] decodes preview frames in the background,
//! [`export`] builds render jobs and draws annotations, and [`render`] produces the output file.
//! The window itself is [`Editor`], which the main application owns and shows every frame.

pub mod export;
#[cfg(windows)]
pub mod player;
pub mod project;
#[cfg(windows)]
pub mod render;

#[cfg(windows)]
pub use ui::Editor;

/// The editor window ([`Editor`]) and its egui drawing and interaction code.
///
/// Kept in a private module so that only [`Editor`] is exported; the module is Windows-only
/// because the preview decoder and renderer use Media Foundation.
#[cfg(windows)]
mod ui {
    use super::{export, player, project};

    use std::path::{Path, PathBuf};

    use ab_glyph::FontArc;
    use egui::{
        Align2, Color32, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, TextureHandle,
        Vec2,
    };

    #[allow(unused_imports)]
    use crate::i18n::{tf, tr};
    use crate::job::{Job, JobStatus};
    use crate::media::{MediaInfo, format_time, mf};
    use crate::settings::OutputFormat;
    use export::ExportOptions;
    use player::Preview;
    use project::{Annotation, AnnotationKind, NRect, Project, Span, TitlePage, Zoom};

    /// What a drag or click on the video preview does.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Tool {
        /// Select and move annotations, or redraw the selected zoom's area.
        Select,
        /// Draw a new annotation of this kind.
        Add(AnnotationKind),
        /// Draw the area of a new zoom effect.
        Zoom,
    }

    /// The item whose properties are shown in the side panel. Indices refer to the
    /// corresponding list in the [`Project`] and are checked before use.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Selected {
        /// Nothing selected.
        None,
        /// An entry of [`Project::annotations`].
        Annotation(usize),
        /// An entry of [`Project::zooms`].
        Zoom(usize),
        /// An entry of [`Project::titles`].
        Title(usize),
        /// An entry of [`Project::cuts`].
        Cut(usize),
        /// An entry of [`Project::speedups`].
        SpeedUp(usize),
    }

    /// What the current drag on the timeline is changing.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum TimelineDrag {
        /// Scrubbing: the playhead follows the pointer.
        Playhead,
        /// Moving the start of the trim.
        TrimStart,
        /// Moving the end of the trim.
        TrimEnd,
        /// Selecting a time range that started at this time (seconds).
        Range(f64),
    }

    /// Pauses found in the audio as `(start, end)` source times in seconds, or the error message
    /// of a failed search.
    type PauseResult = Result<Vec<(f64, f64)>, String>;

    /// The video editor window and all of its state.
    ///
    /// The editor lives for the whole program; [`Editor::open_file`] loads a video and opens the
    /// window, and [`Editor::show`] draws it as a separate egui viewport each frame while
    /// [`Editor::open`] is set. Times are source-video seconds and positions on the video are
    /// normalised (0..=1) frame coordinates.
    pub struct Editor {
        /// Whether the editor window is shown.
        pub open: bool,
        /// Adds zooms around the recorded clicks when a video without a project file opens
        /// (mirrors the application setting).
        pub auto_zoom: bool,
        /// Clicks recorded with the open video, for the "Auto zoom" button; `None` if there are
        /// none.
        clicks: Option<Vec<project::Click>>,
        /// Mouse positions (or clicks for recordings without a pointer log) recorded with the
        /// video, used to preview the vertical / square crop.
        pointer: Vec<project::Click>,
        /// Markers dropped while recording the open video (source seconds).
        markers: Vec<f64>,
        /// The logo image shown on the preview (its file, and the texture if it loaded).
        logo_tex: Option<(PathBuf, Option<egui::TextureHandle>)>,
        /// Crop path computed from `pointer` for the shape it was computed for.
        follow_cache: Option<(export::Shape, Vec<(f32, f32)>)>,
        /// Font for annotations and title pages (preview and export).
        font: FontArc,
        /// The open project, or `None` when no video is loaded.
        project: Option<Project>,
        /// Properties of the open video.
        info: MediaInfo,
        /// Background frame decoder for the open video.
        preview: Option<Preview>,
        /// Texture holding the current video frame.
        texture: Option<TextureHandle>,
        /// Texture with the visible annotations rendered on it, and the cache key it was made
        /// for.
        overlay_tex: Option<(TextureHandle, String)>,
        /// Playhead position in source seconds.
        time: f64,
        /// Whether preview playback is running.
        playing: bool,
        /// Source time at which the current playback started.
        play_from: f64,
        /// Time range selected on the timeline, used by cut, zoom and trim and as the span of
        /// new annotations.
        sel: Option<Span>,
        /// Active tool for the video preview.
        tool: Tool,
        /// Item shown in the properties panel.
        selected: Selected,
        /// Normalised point where the current drag on the preview started.
        drag_start: Option<(f32, f32)>,
        /// Normalised point the current drag on the preview has reached.
        drag_now: Option<(f32, f32)>,
        /// What a drag on the preview moves: `(annotation index, grab offset x, grab offset y)`.
        /// Redrawing a zoom's area is encoded as index `usize::MAX - zoom index`.
        moving: Option<(usize, f32, f32)>,
        /// Drag in progress on the timeline.
        timeline_drag: Option<TimelineDrag>,
        /// Settings of the export panel.
        export_opts: ExportOptions,
        /// Running or finished export job.
        job: Option<Job>,
        /// Pause removal: shortest pause to find, in seconds.
        pause_min: f64,
        /// Pause removal: silence kept next to speech, in seconds.
        pause_keep: f64,
        /// Pause search running on a background thread.
        pause_search: Option<crossbeam_channel::Receiver<PauseResult>>,
        /// Result of the last pause search, cleared when the settings change.
        pause_found: Option<PauseResult>,
        /// Error message shown in the window (failed open, invalid export name).
        message: Option<String>,
        /// Undo history: earlier project states as JSON, oldest first (at most 100).
        undo: Vec<String>,
        /// Redo history: undone project states as JSON.
        redo: Vec<String>,
        /// JSON of the project as last recorded in the history, used to detect edits.
        snapshot: String,
        /// Revision counter, bumped on every edit, undo and redo; part of the overlay cache key.
        rev: u64,
        /// Time of the last frame request, so repeated seeks to the same time are skipped.
        last_seek: f64,
    }

    impl Editor {
        /// Creates a closed editor that will use `font` for annotations and title pages.
        pub fn new(font: FontArc) -> Self {
            Self {
                open: false,
                auto_zoom: false,
                clicks: None,
                pointer: vec![],
                markers: vec![],
                logo_tex: None,
                follow_cache: None,
                font,
                project: None,
                info: MediaInfo::default(),
                preview: None,
                texture: None,
                overlay_tex: None,
                time: 0.0,
                playing: false,
                play_from: 0.0,
                sel: None,
                tool: Tool::Select,
                selected: Selected::None,
                drag_start: None,
                drag_now: None,
                moving: None,
                timeline_drag: None,
                export_opts: ExportOptions::default(),
                job: None,
                pause_min: 0.8,
                pause_keep: 0.15,
                pause_search: None,
                pause_found: None,
                message: None,
                undo: vec![],
                redo: vec![],
                snapshot: String::new(),
                rev: 0,
                last_seek: -1.0,
            }
        }

        /// Replaces the annotation font (when the font setting changes) and drops the cached
        /// overlay so annotations are redrawn with it.
        pub fn set_font(&mut self, font: FontArc) {
            self.font = font;
            self.overlay_tex = None;
        }

        /// Opens the editor window with the video at `path`.
        ///
        /// Loads the saved project (or starts a new one), the recorded clicks and pointer
        /// positions, and starts a preview decoder at time 0. A video opened for the first time
        /// gets automatic zooms around its clicks when [`Editor::auto_zoom`] is set. Undo history
        /// and selection are reset. If the file has no video or cannot be read, the window opens
        /// with an error message instead.
        pub fn open_file(&mut self, path: &Path, ctx: &egui::Context) {
            self.open = true;
            self.message = None;
            match mf::probe(path) {
                Ok(info) if info.has_video => {
                    // No project file yet means this video has never been edited.
                    let fresh = !Project::file_for(path).exists();
                    let mut project = Project::load_or_new(path);
                    self.pause_search = None;
                    self.pause_found = None;
                    self.clicks = project::load_clicks(path).filter(|c| !c.is_empty());
                    self.markers = project::load_markers(path);
                    self.pointer = project::load_pointer(path);
                    self.follow_cache = None;
                    if fresh
                        && self.auto_zoom
                        && let Some(clicks) = &self.clicks
                    {
                        project.zooms = project::auto_zooms(clicks, info.duration);
                    }
                    // Reset all per-video state; the snapshot is the base for undo.
                    self.preview = Some(Preview::new(path.to_path_buf(), info.fps, ctx.clone()));
                    self.info = info;
                    self.snapshot = serde_json::to_string(&project).unwrap_or_default();
                    self.project = Some(project);
                    self.time = 0.0;
                    self.sel = None;
                    self.selected = Selected::None;
                    self.undo.clear();
                    self.redo.clear();
                    self.texture = None;
                    self.overlay_tex = None;
                    self.playing = false;
                    self.last_seek = -1.0;
                    self.seek(0.0);
                }
                Ok(_) => self.message = Some(tf("{} has no video stream.", &[&path.display()])),
                Err(e) => {
                    self.message = Some(tf(
                        "Could not open {}: {}",
                        &[&path.display(), &format!("{e:#}")],
                    ))
                }
            }
        }

        /// Returns the video duration in seconds, never zero (it is used as a divisor).
        fn duration(&self) -> f64 {
            self.info.duration.max(0.001)
        }

        /// Moves the playhead to `t` (clamped to the video) and requests the matching frame.
        ///
        /// During playback, playback restarts from the new position; while paused, a single frame
        /// is decoded unless it was already requested for this time.
        fn seek(&mut self, t: f64) {
            self.time = t.clamp(0.0, self.duration());
            if let Some(p) = &self.preview {
                if self.playing {
                    self.play_from = self.time;
                    p.play(self.time);
                } else if (self.time - self.last_seek).abs() > 0.001 {
                    p.seek(self.time);
                }
            }
            self.last_seek = self.time;
        }

        /// Starts or pauses preview playback. Starting at the very end rewinds to the beginning.
        fn toggle_play(&mut self) {
            let Some(p) = &self.preview else { return };
            if self.playing {
                self.playing = false;
                p.stop();
            } else {
                if self.time >= self.duration() - 0.05 {
                    self.time = 0.0;
                }
                self.playing = true;
                self.play_from = self.time;
                p.play(self.time);
            }
        }

        /// Takes the newest decoded frame from the preview thread and shows it.
        ///
        /// Frames from superseded requests are dropped. During playback the playhead follows the
        /// frames, jumps over cut sections and stops at the end of the video.
        fn receive_frames(&mut self, ctx: &egui::Context) {
            let Some(preview) = &self.preview else { return };
            // Drain the channel, keeping only the newest frame of the current request.
            let mut latest = None;
            let current = preview.generation();
            while let Ok(f) = preview.frames.try_recv() {
                if f.generation == current {
                    latest = Some(f);
                }
            }
            let Some(frame) = latest else { return };
            if frame.eof {
                self.playing = false;
                return;
            }
            // A playback frame that arrives after the user paused is stale.
            if frame.playing && !self.playing {
                return;
            }
            if frame.playing {
                self.time = frame.time;
                // Skip over trimmed and cut sections: restart playback at the next kept span,
                // or stop if there is none.
                if let Some(p) = &self.project {
                    let d = self.info.duration;
                    if p.is_cut(self.time, d) {
                        let next = p.kept_spans(d).into_iter().find(|s| s.start > self.time);
                        match next {
                            Some(s) => {
                                self.time = s.start;
                                self.play_from = s.start;
                                preview.play(s.start);
                            }
                            None => {
                                self.playing = false;
                                preview.stop();
                            }
                        }
                    }
                }
            }
            // Reuse the texture when there is one, so only the pixels are uploaded.
            let img = frame.image;
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("editor-frame", img, egui::TextureOptions::LINEAR))
                }
            }
        }

        /// Records edits in the undo history and auto-saves the project.
        ///
        /// Called once per frame. Edits are detected by comparing the project's JSON with the
        /// last snapshot; while a mouse button is held, nothing is recorded, so a whole drag
        /// becomes a single undo step.
        fn track_changes(&mut self, ctx: &egui::Context) {
            let Some(p) = &self.project else { return };
            if ctx.input(|i| i.pointer.any_down()) {
                return;
            }
            let json = serde_json::to_string(p).unwrap_or_default();
            if json != self.snapshot {
                // The previous state goes on the undo stack (capped at 100 entries) and any
                // redo history becomes invalid.
                let prev = std::mem::replace(&mut self.snapshot, json);
                self.undo.push(prev);
                if self.undo.len() > 100 {
                    self.undo.remove(0);
                }
                self.redo.clear();
                self.rev += 1;
                p.save();
            }
        }

        /// Restores the previous project state, saves it and makes the current one redoable.
        fn undo(&mut self) {
            if let Some(prev) = self.undo.pop()
                && let Ok(p) = serde_json::from_str::<Project>(&prev)
            {
                self.redo.push(std::mem::replace(&mut self.snapshot, prev));
                p.save();
                self.project = Some(p);
                self.selected = Selected::None;
                self.rev += 1;
            }
        }

        /// Re-applies the most recently undone project state and saves it.
        fn redo(&mut self) {
            if let Some(next) = self.redo.pop()
                && let Ok(p) = serde_json::from_str::<Project>(&next)
            {
                self.undo.push(std::mem::replace(&mut self.snapshot, next));
                p.save();
                self.project = Some(p);
                self.selected = Selected::None;
                self.rev += 1;
            }
        }

        /// Draws the editor in its own native window (an immediate egui viewport) if it is open.
        ///
        /// Also handles the window's close button, incoming preview frames, keyboard shortcuts
        /// and change tracking, and keeps repainting while playback or an export runs.
        pub fn show(&mut self, ctx: &egui::Context) {
            if !self.open {
                return;
            }
            let title = match &self.project {
                Some(p) => tf(
                    "Video Editor — {}",
                    &[&p.source
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()],
                ),
                None => tr("Video Editor").into(),
            };
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("video-editor"),
                crate::geometry::centered(
                    egui::ViewportBuilder::default()
                        .with_title(title)
                        .with_inner_size([1180.0, 760.0])
                        .with_min_inner_size([760.0, 480.0])
                        .with_visible(false),
                    [1180.0, 760.0],
                ),
                |ui, _class| {
                    // The window is created hidden and revealed once laid out, to avoid a flash.
                    crate::util::reveal_when_ready(ui.ctx());
                    let ctx = ui.ctx().clone();
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.close();
                        crate::util::forget_reveal(&ctx, ctx.viewport_id());
                        return;
                    }
                    // Frames first so the UI draws the newest picture; change tracking last so
                    // it sees this frame's edits.
                    self.receive_frames(&ctx);
                    self.handle_keys(&ctx);
                    self.ui(ui);
                    self.track_changes(&ctx);
                    // Keep the progress bar and playhead moving without input events.
                    if self.playing || self.job.as_ref().is_some_and(|j| !j.is_finished()) {
                        ctx.request_repaint_after(std::time::Duration::from_millis(30));
                    }
                },
            );
        }

        /// Saves the project, stops the preview decoder and releases the video and textures.
        fn close(&mut self) {
            if let Some(p) = &self.project {
                p.save();
            }
            self.open = false;
            self.playing = false;
            self.preview = None;
            self.texture = None;
            self.overlay_tex = None;
            self.project = None;
        }

        /// Draws the project's logo on the preview where the export will put it.
        ///
        /// `img_rect` is where the video is drawn on screen; the logo's width is a fraction of
        /// it and the margin from the corner scales with its height.
        fn paint_logo(&mut self, painter: &egui::Painter, img_rect: Rect) {
            let Some(wm) = self.project.as_ref().and_then(|p| p.watermark.clone()) else {
                return;
            };
            // Load the image once per file; a failed load is remembered so it is not retried
            // on every frame.
            if self
                .logo_tex
                .as_ref()
                .is_none_or(|(path, _)| *path != wm.path)
            {
                let tex = image::open(&wm.path).ok().map(|img| {
                    let img = img.to_rgba8();
                    let size = [img.width() as usize, img.height() as usize];
                    let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                    painter
                        .ctx()
                        .load_texture("editor-logo", ci, egui::TextureOptions::LINEAR)
                });
                self.logo_tex = Some((wm.path.clone(), tex));
            }
            let Some((_, Some(tex))) = &self.logo_tex else {
                return;
            };
            let [iw, ih] = tex.size();
            let lw = img_rect.width() * wm.size;
            let size = Vec2::new(lw, lw * ih as f32 / iw.max(1) as f32);
            let m = (img_rect.height() / 30.0).max(3.0);
            use crate::settings::WebcamPosition as C;
            let min = match wm.corner {
                C::TopLeft => img_rect.min + Vec2::splat(m),
                C::TopRight => Pos2::new(img_rect.right() - m - size.x, img_rect.top() + m),
                C::BottomLeft => Pos2::new(img_rect.left() + m, img_rect.bottom() - m - size.y),
                C::BottomRight => img_rect.max - Vec2::splat(m) - size,
            };
            painter.image(
                tex.id(),
                Rect::from_min_size(min, size),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::from_white_alpha((wm.opacity.clamp(0.0, 1.0) * 255.0) as u8),
            );
        }

        /// Outlines the vertical / square export crop on the preview and dims the rest.
        ///
        /// `img_rect` is where the video is drawn on screen and `uv` the part of the frame shown
        /// in it (smaller than the whole frame while a zoom is active). The crop follows the
        /// recorded pointer path, as it will in the export. Does nothing for the original shape.
        fn paint_shape_frame(&mut self, painter: &egui::Painter, img_rect: Rect, uv: Rect) {
            let shape = self.export_opts.shape;
            if shape == export::Shape::Original {
                return;
            }
            let (_, _, fw, fh) = shape.frame(self.info.width, self.info.height);
            // The crop path covers the whole video, so compute it once per shape.
            if self.follow_cache.as_ref().is_none_or(|(s, _)| *s != shape) {
                let path = project::follow_path(&self.pointer, self.info.duration, fw, fh);
                self.follow_cache = Some((shape, path));
            }
            let Some((_, path)) = &self.follow_cache else {
                return;
            };
            let (cx, cy) = project::follow_at(path, self.time);
            // Maps source fractions to screen points through the visible (possibly zoomed) part.
            let to_screen = |x: f32, y: f32| {
                Pos2::new(
                    img_rect.left() + (x - uv.min.x) / uv.width() * img_rect.width(),
                    img_rect.top() + (y - uv.min.y) / uv.height() * img_rect.height(),
                )
            };
            let frame = Rect::from_min_max(
                to_screen(cx - fw / 2.0, cy - fh / 2.0),
                to_screen(cx + fw / 2.0, cy + fh / 2.0),
            )
            .intersect(img_rect);
            // Dim the four bands around the crop: left, right, above and below.
            let dim = Color32::from_black_alpha(140);
            for part in [
                Rect::from_min_max(img_rect.min, Pos2::new(frame.left(), img_rect.bottom())),
                Rect::from_min_max(Pos2::new(frame.right(), img_rect.top()), img_rect.max),
                Rect::from_min_max(
                    Pos2::new(frame.left(), img_rect.top()),
                    Pos2::new(frame.right(), frame.top()),
                ),
                Rect::from_min_max(
                    Pos2::new(frame.left(), frame.bottom()),
                    Pos2::new(frame.right(), img_rect.bottom()),
                ),
            ] {
                if part.is_positive() {
                    painter.rect_filled(part, 0.0, dim);
                }
            }
            painter.rect_stroke(
                frame,
                0.0,
                Stroke::new(1.5, Color32::WHITE),
                egui::StrokeKind::Inside,
            );
        }

        /// Handles the editor's keyboard shortcuts: Space (play / pause), Delete or Backspace
        /// (delete the selected item), Ctrl+Z / Ctrl+Y or Ctrl+Shift+Z (undo / redo), ← / →
        /// (one frame back / forward) and Home / End.
        ///
        /// Ignored while a text field has focus, so typing does not trigger them.
        fn handle_keys(&mut self, ctx: &egui::Context) {
            if ctx.egui_wants_keyboard_input() {
                return;
            }
            let (space, del, undo, redo, left, right, home, end) = ctx.input(|i| {
                (
                    i.key_pressed(Key::Space),
                    i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
                    i.modifiers.command && !i.modifiers.shift && i.key_pressed(Key::Z),
                    i.modifiers.command
                        && (i.key_pressed(Key::Y) || (i.modifiers.shift && i.key_pressed(Key::Z))),
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                )
            });
            let step = 1.0 / self.info.fps.max(1.0);
            if space {
                self.toggle_play();
            }
            if del {
                self.delete_selected();
            }
            if undo {
                self.undo();
            }
            if redo {
                self.redo();
            }
            if left {
                self.seek(self.time - step);
            }
            if right {
                self.seek(self.time + step);
            }
            if home {
                self.seek(0.0);
            }
            if end {
                self.seek(self.duration());
            }
        }

        /// Removes the selected annotation, zoom, title page or cut (restoring the cut section)
        /// and clears the selection.
        fn delete_selected(&mut self) {
            let Some(p) = &mut self.project else { return };
            match self.selected {
                Selected::Annotation(i) if i < p.annotations.len() => {
                    p.annotations.remove(i);
                }
                Selected::Zoom(i) if i < p.zooms.len() => {
                    p.zooms.remove(i);
                }
                Selected::Title(i) if i < p.titles.len() => {
                    p.titles.remove(i);
                }
                Selected::Cut(i) if i < p.cuts.len() => {
                    p.cuts.remove(i);
                }
                Selected::SpeedUp(i) if i < p.speedups.len() => {
                    p.speedups.remove(i);
                }
                _ => {}
            }
            self.selected = Selected::None;
        }

        /// Lays out the window: toolbar on top, then (with a video loaded) the properties panel
        /// on the right, transport and timeline at the bottom and the video preview in the
        /// centre. Without a video, the centre shows a hint or the last error.
        fn ui(&mut self, ui: &mut egui::Ui) {
            egui::Panel::top("editor-toolbar").show(ui, |ui| {
                ui.add_space(4.0);
                self.toolbar(ui);
                ui.add_space(4.0);
            });

            if self.project.is_none() {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            self.message
                                .clone()
                                .unwrap_or_else(|| tr("Open a video to start editing.").into()),
                        );
                    });
                });
                return;
            }

            egui::Panel::right("editor-props")
                .default_size(290.0)
                .resizable(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
                });
            egui::Panel::bottom("editor-timeline")
                .exact_size(262.0)
                .show(ui, |ui| {
                    self.transport(ui);
                    self.timeline(ui);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(Color32::from_gray(24)))
                .show(ui, |ui| self.preview_area(ui));
        }

        /// Draws the toolbar: open, undo / redo, the drawing tools, zoom, title page and (when
        /// the video has recorded clicks) automatic zoom.
        fn toolbar(&mut self, ui: &mut egui::Ui) {
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("📂 Open…")).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Video", &["mp4", "mov", "m4v", "avi", "wmv", "mkv"])
                        .pick_file()
                    {
                        let ctx = ui.ctx().clone();
                        self.open_file(&path, &ctx);
                    }
                ui.add_enabled_ui(!self.undo.is_empty(), |ui| {
                    if ui.button(tr("↺ Undo")).on_hover_text("Ctrl+Z").clicked() {
                        self.undo();
                    }
                });
                ui.add_enabled_ui(!self.redo.is_empty(), |ui| {
                    if ui.button(tr("↻ Redo")).on_hover_text("Ctrl+Y").clicked() {
                        self.redo();
                    }
                });
                ui.separator();
                // The editing tools only make sense with a video loaded.
                if self.project.is_none() {
                    return;
                }
                ui.label(tr("Draw:"));
                if ui
                    .selectable_label(self.tool == Tool::Select, tr("⬉ Select"))
                    .clicked()
                {
                    self.tool = Tool::Select;
                }
                for kind in AnnotationKind::ALL {
                    let icon = match kind {
                        AnnotationKind::Text => "T",
                        AnnotationKind::Callout => "💬",
                        AnnotationKind::Arrow => "↗",
                        AnnotationKind::Rectangle => "🔲",
                        AnnotationKind::Ellipse => "⭕",
                        AnnotationKind::Highlight => "🖊",
                        AnnotationKind::Blur => "💧",
                        AnnotationKind::Step => "🔢",
                    };
                    if ui
                        .selectable_label(
                            self.tool == Tool::Add(kind),
                            format!("{icon} {}", kind.label()),
                        )
                        .on_hover_text(tr("Drag on the video to place it at the current time"))
                        .clicked()
                    {
                        self.tool = Tool::Add(kind);
                    }
                }
                ui.separator();
                if ui
                .selectable_label(self.tool == Tool::Zoom, tr("🔍 Zoom"))
                .on_hover_text(
                    tr("Drag a rectangle on the video to zoom into it during the selected time range"),
                )
                .clicked()
            {
                self.tool = Tool::Zoom;
            }
                if ui
                    .button(tr("🎬 Title page"))
                    .on_hover_text(tr("Insert a title page at the playhead"))
                    .clicked()
                {
                    // Insert at the playhead and select the new page for editing.
                    let t = self.time;
                    if let Some(p) = &mut self.project {
                        p.titles.push(TitlePage::new(t));
                        self.selected = Selected::Title(p.titles.len() - 1);
                    }
                }
                if let Some(clicks) = &self.clicks
                    && ui
                        .button(tr("✨ Auto zoom"))
                        .on_hover_text(tf(
                            "Replace the zooms with zooms on the {} recorded clicks",
                            &[&clicks.len()],
                        ))
                        .clicked()
                    {
                        // Replaces all existing zooms; the change can be undone.
                        let d = self.info.duration;
                        if let Some(p) = &mut self.project {
                            p.zooms = project::auto_zooms(clicks, d);
                            self.selected = Selected::None;
                        }
                    }
            });
        }

        /// Draws the transport row above the timeline: playback and frame-step buttons, the
        /// time display, mark in / out and the actions on the selected time range.
        fn transport(&mut self, ui: &mut egui::Ui) {
            ui.add_space(4.0);
            // Wraps onto a second line when the window is too narrow for all the buttons.
            ui.horizontal_wrapped(|ui| {
                let step = 1.0 / self.info.fps.max(1.0);
                if ui.button("⏮").on_hover_text(tr("Start (Home)")).clicked() {
                    self.seek(0.0);
                }
                if ui
                    .button("◀")
                    .on_hover_text(tr("Previous frame (←)"))
                    .clicked()
                {
                    self.seek(self.time - step);
                }
                if ui
                    .button(if self.playing {
                        tr("⏸ Pause")
                    } else {
                        tr("▶ Play")
                    })
                    .on_hover_text(tr("Space"))
                    .clicked()
                {
                    self.toggle_play();
                }
                if ui.button("▶").on_hover_text(tr("Next frame (→)")).clicked() {
                    self.seek(self.time + step);
                }
                if ui.button("⏭").on_hover_text(tr("End")).clicked() {
                    self.seek(self.duration());
                }
                ui.monospace(format!(
                    "{} / {}",
                    format_time(self.time),
                    format_time(self.info.duration)
                ));
                ui.separator();
                // Mark in / out set one end of the selection at the playhead, keeping the other
                // end (or the video's start / end) and a minimum length of 0.1 s.
                if ui.button(tr("[ Mark in")).clicked() {
                    let end = self.sel.map(|s| s.end).unwrap_or(self.duration());
                    self.sel = Some(Span::new(self.time, end.max(self.time + 0.1)));
                }
                if ui.button(tr("Mark out ]")).clicked() {
                    let start = self.sel.map(|s| s.start).unwrap_or(0.0);
                    self.sel = Some(Span::new(start.min(self.time - 0.1).max(0.0), self.time));
                }
                // The range actions need a selection longer than 50 ms.
                let has_sel = self.sel.is_some_and(|s| s.len() > 0.05);
                // Each range action is enabled on its own (not as a group) so the row can
                // wrap between buttons on narrow windows.
                if ui
                    .add_enabled(has_sel, egui::Button::new(tr("✂ Cut selection")))
                    .on_hover_text(tr("Remove the selected section"))
                    .clicked()
                    && let (Some(p), Some(s)) = (&mut self.project, self.sel)
                {
                    p.cuts.push(s);
                    self.selected = Selected::Cut(p.cuts.len() - 1);
                    self.sel = None;
                }
                if ui
                    .add_enabled(has_sel, egui::Button::new(tr("🔍 Zoom selection")))
                    .on_hover_text(tr("Zoom into the centre during the selection"))
                    .clicked()
                {
                    // A 2× zoom on the centre of the frame, ready to be adjusted.
                    if let (Some(p), Some(s)) = (&mut self.project, self.sel) {
                        p.zooms.push(Zoom {
                            span: s,
                            rect: NRect {
                                x: 0.25,
                                y: 0.25,
                                w: 0.5,
                                h: 0.5,
                            },
                        });
                        self.selected = Selected::Zoom(p.zooms.len() - 1);
                    }
                }
                ui.menu_button(tr("⏩ Speed up"), |ui| {
                    if !has_sel {
                        ui.label(tr("Select a range on the ruler first."));
                        return;
                    }
                    ui.label(egui::RichText::new(tr("Play the selection faster:")).weak());
                    for f in [2.0f32, 4.0, 8.0] {
                        if ui.button(format!("{}×", f as u32)).clicked() {
                            if let (Some(p), Some(s)) = (&mut self.project, self.sel) {
                                p.speedups.push(project::SpeedUp { span: s, factor: f });
                                self.selected = Selected::SpeedUp(p.speedups.len() - 1);
                                self.sel = None;
                            }
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text(tr(
                    "Fast-forward through a slow part (it is muted in the export)",
                ));
                if ui
                    .add_enabled(has_sel, egui::Button::new(tr("⬌ Trim to selection")))
                    .clicked()
                    && let (Some(p), Some(s)) = (&mut self.project, self.sel)
                {
                    p.trim = Some(s);
                    self.sel = None;
                }
                if ui
                    .add_enabled(has_sel, egui::Button::new(tr("✖ Clear selection")))
                    .clicked()
                {
                    self.sel = None;
                }
                if let Some(s) = self.sel {
                    ui.label(tf(
                        "Selection: {} – {}",
                        &[&format_time(s.start), &format_time(s.end)],
                    ));
                }
            });
        }

        /// Draws the timeline and handles its mouse interaction.
        ///
        /// From top to bottom it shows a time ruler, the main track (kept parts in green, cuts in
        /// red, yellow trim handles), and rows for zooms, annotations and title pages. Dragging
        /// scrubs the playhead, moves a trim handle, or (on the ruler or with Shift) selects a
        /// time range; clicking selects an item or moves the playhead.
        fn timeline(&mut self, ui: &mut egui::Ui) {
            let dur = self.duration();
            let avail = ui.available_size();
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(avail.x, avail.y - 6.0), Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            let inner = rect.shrink2(Vec2::new(10.0, 2.0));
            // Conversions between source time and x position on the timeline.
            let to_x = |t: f64| inner.left() + (t / dur) as f32 * inner.width();
            let to_t = |x: f32| (((x - inner.left()) / inner.width()) as f64 * dur).clamp(0.0, dur);

            // Vertical layout of the ruler, main track and item rows.
            painter.rect_filled(rect, 4.0, Color32::from_gray(30));
            let ruler = Rect::from_min_max(inner.min, Pos2::new(inner.right(), inner.top() + 20.0));
            let track = Rect::from_min_max(
                Pos2::new(inner.left(), ruler.bottom() + 4.0),
                Pos2::new(inner.right(), ruler.bottom() + 44.0),
            );
            let row_h = 30.0;
            let zoom_row = Rect::from_min_size(
                Pos2::new(inner.left(), track.bottom() + 6.0),
                Vec2::new(inner.width(), row_h),
            );
            let ann_row = Rect::from_min_size(
                Pos2::new(inner.left(), zoom_row.bottom() + 4.0),
                Vec2::new(inner.width(), row_h),
            );
            let title_row = Rect::from_min_size(
                Pos2::new(inner.left(), ann_row.bottom() + 4.0),
                Vec2::new(inner.width(), row_h),
            );

            // Ruler ticks: the smallest "nice" interval that leaves about 80 px between labels.
            let target_ticks = (inner.width() / 80.0).max(2.0) as f64;
            let raw = dur / target_ticks;
            let nice = [
                0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
            ];
            let step = nice.iter().copied().find(|s| *s >= raw).unwrap_or(1200.0);
            let mut t = 0.0;
            while t <= dur + 1e-6 {
                let x = to_x(t);
                painter.line_segment(
                    [
                        Pos2::new(x, ruler.bottom() - 6.0),
                        Pos2::new(x, ruler.bottom()),
                    ],
                    Stroke::new(1.0, Color32::GRAY),
                );
                painter.text(
                    Pos2::new(x + 2.0, ruler.top()),
                    Align2::LEFT_TOP,
                    format_time(t),
                    FontId::proportional(11.5),
                    Color32::GRAY,
                );
                t += step;
            }

            let Some(project) = self.project.as_ref() else {
                return;
            };
            let trim = project.trim_span(dur);

            // Main track: grey background (trimmed), kept spans in green, cuts in red on top.
            painter.rect_filled(track, 3.0, Color32::from_rgb(70, 70, 70));
            for s in project.kept_spans(dur) {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(s.start), track.top()),
                    Pos2::new(to_x(s.end), track.bottom()),
                );
                painter.rect_filled(r, 2.0, Color32::from_rgb(46, 125, 50));
            }
            for (i, c) in project.cuts.iter().enumerate() {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(c.start), track.top()),
                    Pos2::new(to_x(c.end), track.bottom()),
                );
                let sel = self.selected == Selected::Cut(i);
                painter.rect_filled(r, 2.0, Color32::from_rgba_unmultiplied(198, 40, 40, 200));
                painter.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    "✂",
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
                if sel {
                    painter.rect_stroke(
                        r,
                        2.0,
                        Stroke::new(2.0, Color32::WHITE),
                        StrokeKind::Inside,
                    );
                }
            }
            // Sped-up sections: orange bands along the bottom of the main track.
            for (i, u) in project.speedups.iter().enumerate() {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(u.span.start), track.bottom() - 14.0),
                    Pos2::new(to_x(u.span.end), track.bottom()),
                );
                painter.rect_filled(r, 2.0, Color32::from_rgba_unmultiplied(245, 124, 0, 230));
                painter.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    format!("▶▶ {}×", u.factor as u32),
                    FontId::proportional(10.5),
                    Color32::WHITE,
                );
                if self.selected == Selected::SpeedUp(i) {
                    painter.rect_stroke(
                        r,
                        2.0,
                        Stroke::new(2.0, Color32::WHITE),
                        StrokeKind::Inside,
                    );
                }
            }
            // Markers dropped while recording: amber flags on the ruler with a line down
            // through the main track.
            for m in &self.markers {
                let x = to_x(*m);
                let amber = Color32::from_rgb(255, 193, 7);
                painter.line_segment(
                    [Pos2::new(x, ruler.top()), Pos2::new(x, track.bottom())],
                    Stroke::new(1.0, amber),
                );
                painter.add(Shape::convex_polygon(
                    vec![
                        Pos2::new(x, ruler.top()),
                        Pos2::new(x + 9.0, ruler.top() + 4.0),
                        Pos2::new(x, ruler.top() + 8.0),
                    ],
                    amber,
                    Stroke::NONE,
                ));
            }
            // Trim handles at both ends of the kept range.
            for t in [trim.start, trim.end] {
                let x = to_x(t);
                let h = Rect::from_center_size(
                    Pos2::new(x, track.center().y),
                    Vec2::new(8.0, track.height() + 8.0),
                );
                painter.rect_filled(h, 2.0, Color32::from_rgb(255, 214, 0));
            }

            // Item rows. Each drawn item is also recorded in `hits` for click selection.
            let label = |r: Rect, text: &str| {
                painter.text(
                    Pos2::new(r.left() + 2.0, r.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    FontId::proportional(11.5),
                    Color32::from_gray(110),
                );
            };
            label(zoom_row, tr("Zoom"));
            label(ann_row, tr("Annotations"));
            label(title_row, tr("Titles"));
            let mut hits: Vec<(Rect, Selected)> = vec![];
            for (i, z) in project.zooms.iter().enumerate() {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(z.span.start), zoom_row.top()),
                    Pos2::new(
                        to_x(z.span.end).max(to_x(z.span.start) + 4.0),
                        zoom_row.bottom(),
                    ),
                );
                painter.rect_filled(r, 3.0, Color32::from_rgb(30, 136, 229));
                painter.text(
                    r.left_center() + Vec2::new(4.0, 0.0),
                    Align2::LEFT_CENTER,
                    "🔍",
                    FontId::proportional(11.0),
                    Color32::WHITE,
                );
                if self.selected == Selected::Zoom(i) {
                    painter.rect_stroke(
                        r,
                        3.0,
                        Stroke::new(2.0, Color32::WHITE),
                        StrokeKind::Inside,
                    );
                }
                hits.push((r, Selected::Zoom(i)));
            }
            // Annotations alternate between two lanes so overlapping ones stay clickable.
            let lanes = 2usize;
            for (i, a) in project.annotations.iter().enumerate() {
                let lane = i % lanes;
                let lh = row_h / lanes as f32;
                let r = Rect::from_min_max(
                    Pos2::new(to_x(a.span.start), ann_row.top() + lane as f32 * lh),
                    Pos2::new(
                        to_x(a.span.end).max(to_x(a.span.start) + 4.0),
                        ann_row.top() + (lane + 1) as f32 * lh - 1.0,
                    ),
                );
                painter.rect_filled(r, 2.0, Color32::from_rgb(67, 160, 71));
                painter.text(
                    r.left_center() + Vec2::new(3.0, 0.0),
                    Align2::LEFT_CENTER,
                    a.kind.label(),
                    FontId::proportional(11.0),
                    Color32::WHITE,
                );
                if self.selected == Selected::Annotation(i) {
                    painter.rect_stroke(
                        r,
                        2.0,
                        Stroke::new(2.0, Color32::WHITE),
                        StrokeKind::Inside,
                    );
                }
                hits.push((r, Selected::Annotation(i)));
            }
            // Title pages are diamonds at their insertion time.
            for (i, t) in project.titles.iter().enumerate() {
                let x = to_x(t.at);
                let r = Rect::from_center_size(
                    Pos2::new(x, title_row.center().y),
                    Vec2::new(14.0, row_h),
                );
                painter.add(Shape::convex_polygon(
                    vec![
                        Pos2::new(x, r.top()),
                        Pos2::new(r.right(), r.center().y),
                        Pos2::new(x, r.bottom()),
                        Pos2::new(r.left(), r.center().y),
                    ],
                    Color32::from_rgb(142, 36, 170),
                    if self.selected == Selected::Title(i) {
                        Stroke::new(2.0, Color32::WHITE)
                    } else {
                        Stroke::NONE
                    },
                ));
                hits.push((r, Selected::Title(i)));
            }
            // Cuts on the main track are selectable too (to restore or adjust them).
            for (i, c) in project.cuts.iter().enumerate() {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(c.start), track.top()),
                    Pos2::new(to_x(c.end), track.bottom()),
                );
                hits.push((r, Selected::Cut(i)));
            }
            // Speed-up bands lie on top of the track, so they are hit before cuts.
            for (i, u) in project.speedups.iter().enumerate() {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(u.span.start), track.bottom() - 14.0),
                    Pos2::new(to_x(u.span.end), track.bottom()),
                );
                hits.push((r, Selected::SpeedUp(i)));
            }

            // Selected time range, over the ruler and main track.
            if let Some(s) = self.sel {
                let r = Rect::from_min_max(
                    Pos2::new(to_x(s.start), inner.top()),
                    Pos2::new(to_x(s.end), track.bottom()),
                );
                painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40));
                painter.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0, Color32::from_gray(200)),
                    StrokeKind::Inside,
                );
            }

            // Playhead: a red line with a triangle on top.
            let px = to_x(self.time);
            painter.line_segment(
                [Pos2::new(px, inner.top()), Pos2::new(px, inner.bottom())],
                Stroke::new(2.0, Color32::from_rgb(255, 82, 82)),
            );
            painter.add(Shape::convex_polygon(
                vec![
                    Pos2::new(px - 6.0, inner.top()),
                    Pos2::new(px + 6.0, inner.top()),
                    Pos2::new(px, inner.top() + 8.0),
                ],
                Color32::from_rgb(255, 82, 82),
                Stroke::NONE,
            ));

            // Interaction. A drag decides what it changes from where it started (the press
            // origin, since the pointer has already moved when the drag is recognised).
            let shift = ui.input(|i| i.modifiers.shift);
            if resp.drag_started()
                && let Some(pos) = ui
                    .input(|i| i.pointer.press_origin())
                    .or_else(|| resp.interact_pointer_pos())
            {
                let near = |t: f64| {
                    (to_x(t) - pos.x).abs() < 7.0
                        && pos.y >= track.top() - 4.0
                        && pos.y <= track.bottom() + 4.0
                };
                self.timeline_drag = Some(if near(trim.start) {
                    TimelineDrag::TrimStart
                } else if near(trim.end) {
                    TimelineDrag::TrimEnd
                } else if ruler.contains(pos) || shift {
                    TimelineDrag::Range(to_t(pos.x))
                } else {
                    TimelineDrag::Playhead
                });
                // Editing the timeline pauses playback.
                if self.playing {
                    self.toggle_play();
                }
            }
            // Apply the drag; trim handles keep at least 0.1 s between them and the preview
            // follows the handle being moved.
            if resp.dragged()
                && let (Some(drag), Some(pos)) = (self.timeline_drag, resp.interact_pointer_pos())
            {
                let t = to_t(pos.x);
                match drag {
                    TimelineDrag::Playhead => self.seek(t),
                    TimelineDrag::TrimStart => {
                        if let Some(p) = &mut self.project {
                            p.trim = Some(Span::new(t.min(trim.end - 0.1), trim.end));
                        }
                        self.seek(t);
                    }
                    TimelineDrag::TrimEnd => {
                        if let Some(p) = &mut self.project {
                            p.trim = Some(Span::new(trim.start, t.max(trim.start + 0.1)));
                        }
                        self.seek(t);
                    }
                    TimelineDrag::Range(start) => {
                        self.sel = Some(Span::new(start, t));
                    }
                }
            }
            // A range shorter than 50 ms counts as an accidental click and is dropped.
            if resp.drag_stopped() {
                self.timeline_drag = None;
                if self.sel.is_some_and(|s| s.len() < 0.05) {
                    self.sel = None;
                }
            }
            // Click: select the topmost item under the pointer (moving the playhead into a
            // selected annotation so it is visible), or move the playhead there.
            if resp.clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                // A click on a marker flag jumps to the marker.
                let marker = self
                    .markers
                    .iter()
                    .copied()
                    .find(|m| ruler.contains(pos) && (to_x(*m) - pos.x).abs() < 7.0);
                if let Some(m) = marker {
                    self.seek(m);
                } else if let Some((_, s)) = hits.iter().rev().find(|(r, _)| r.contains(pos)) {
                    self.selected = *s;
                    if let Selected::Annotation(i) = s
                        && let Some(a) = self.project.as_ref().and_then(|p| p.annotations.get(*i))
                        && !a.span.contains(self.time)
                    {
                        let t = a.span.start;
                        self.seek(t);
                    }
                } else {
                    self.selected = Selected::None;
                    self.seek(to_t(pos.x));
                }
            }
            resp.on_hover_text(
            tr("Drag: scrub • Drag yellow handles: trim • Drag on ruler / Shift+drag: select range"),
        );
        }

        /// Draws the video preview and handles drawing and moving items on it.
        ///
        /// The frame is fitted into the available space, with the active zoom applied, the export
        /// crop outlined, annotations overlaid and zoom areas being edited outlined. Drags draw
        /// new annotations or zoom areas, move annotations, or redraw the selected zoom's area;
        /// clicks select annotations or place text, callouts and step numbers.
        fn preview_area(&mut self, ui: &mut egui::Ui) {
            // Fit the video into the panel, keeping its aspect ratio.
            let avail = ui.available_rect_before_wrap();
            let (vw, vh) = (
                self.info.width.max(1) as f32,
                self.info.height.max(1) as f32,
            );
            let scale = (avail.width() / vw).min(avail.height() / vh);
            let size = Vec2::new(vw * scale, vh * scale);
            let img_rect = Rect::from_center_size(avail.center(), size);
            let resp = ui.allocate_rect(avail, Sense::click_and_drag());
            let painter = ui.painter_at(avail);

            // Show the zoomed view when the playhead is inside a zoom, except while that zoom is
            // selected or the zoom tool is active: then the whole frame is needed to edit it.
            let active_zoom = self.project.as_ref().and_then(|p| {
                p.zooms
                    .iter()
                    .enumerate()
                    .find(|(_, z)| z.span.contains(self.time))
                    .map(|(i, z)| (i, z.rect_at(self.time)))
            });
            let show_zoomed = active_zoom.is_some_and(|(i, _)| self.selected != Selected::Zoom(i))
                && self.tool != Tool::Zoom;
            let uv = match active_zoom {
                Some((_, r)) if show_zoomed => {
                    Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h))
                }
                _ => Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            };
            if let Some(tex) = &self.texture {
                painter.image(tex.id(), img_rect, uv, Color32::WHITE);
                self.paint_logo(&painter, img_rect);
                self.paint_shape_frame(&painter, img_rect, uv);
            } else {
                painter.rect_filled(img_rect, 0.0, Color32::BLACK);
                painter.text(
                    img_rect.center(),
                    Align2::CENTER_CENTER,
                    tr("Loading…"),
                    FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }

            // A selected title page is previewed when the playhead is at its insertion time.
            // This is a quick approximation with egui text; the export uses `render_title`.
            let title_here = self.project.as_ref().and_then(|p| {
                p.titles
                    .iter()
                    .position(|t| (t.at - self.time).abs() < 0.05)
            });
            if let Some(ti) = title_here
                && self.selected == Selected::Title(ti)
                && let Some(p) = &self.project
            {
                let t = &p.titles[ti];
                let bg = Color32::from_rgba_unmultiplied(
                    t.background[0],
                    t.background[1],
                    t.background[2],
                    255,
                );
                let fg = Color32::from_rgba_unmultiplied(
                    t.foreground[0],
                    t.foreground[1],
                    t.foreground[2],
                    255,
                );
                painter.rect_filled(img_rect, 0.0, bg);
                painter.text(
                    img_rect.center() - Vec2::new(0.0, size.y * 0.04),
                    Align2::CENTER_CENTER,
                    &t.title,
                    FontId::proportional(size.y * 0.09),
                    fg,
                );
                painter.text(
                    img_rect.center() + Vec2::new(0.0, size.y * 0.07),
                    Align2::CENTER_CENTER,
                    &t.subtitle,
                    FontId::proportional(size.y * 0.045),
                    fg,
                );
            }

            // Annotations are positioned on the whole frame, so they are hidden in the zoomed
            // view rather than drawn in the wrong place.
            if !show_zoomed {
                self.paint_annotations(ui.ctx(), &painter, img_rect);
            }

            // Outline the zoom areas being edited: the selected zoom, or with the zoom tool every
            // zoom active at the playhead.
            if let Some(p) = &self.project {
                for (i, z) in p.zooms.iter().enumerate() {
                    if self.selected == Selected::Zoom(i)
                        || (self.tool == Tool::Zoom && z.span.contains(self.time))
                    {
                        let r = Rect::from_min_size(
                            img_rect.min + Vec2::new(z.rect.x * size.x, z.rect.y * size.y),
                            Vec2::new(z.rect.w * size.x, z.rect.h * size.y),
                        );
                        painter.rect_stroke(
                            r,
                            0.0,
                            Stroke::new(2.0, Color32::from_rgb(30, 136, 229)),
                            StrokeKind::Outside,
                        );
                        painter.text(
                            r.left_top(),
                            Align2::LEFT_BOTTOM,
                            tr("Zoom area"),
                            FontId::proportional(12.0),
                            Color32::from_rgb(100, 181, 246),
                        );
                    }
                }
            }

            // Interaction. Screen points are converted to normalised frame coordinates.
            let to_norm = |p: Pos2| {
                (
                    ((p.x - img_rect.left()) / size.x).clamp(0.0, 1.0),
                    ((p.y - img_rect.top()) / size.y).clamp(0.0, 1.0),
                )
            };
            if resp.drag_started()
                && let Some(pos) = ui
                    .input(|i| i.pointer.press_origin())
                    .or_else(|| resp.interact_pointer_pos())
            {
                let n = to_norm(pos);
                match self.tool {
                    // Select tool: grab the annotation under the pointer (remembering the
                    // grab offset), or else start redrawing the selected zoom's area.
                    Tool::Select => {
                        if let Some(i) = self.hit_annotation(n) {
                            self.selected = Selected::Annotation(i);
                            let a = &self.project.as_ref().unwrap().annotations[i];
                            self.moving = Some((i, n.0 - a.x, n.1 - a.y));
                        } else if let Selected::Zoom(zi) = self.selected {
                            self.drag_start = Some(n);
                            self.moving = Some((usize::MAX - zi, 0.0, 0.0));
                        }
                    }
                    // Drawing tools: start a rubber band.
                    _ => self.drag_start = Some(n),
                }
            }
            // Move the grabbed annotation (it may stick out of the frame by up to half its
            // width) or reshape the zoom area.
            if resp.dragged()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let n = to_norm(pos);
                self.drag_now = Some(n);
                if let (Some((i, ox, oy)), Some(p)) = (self.moving, self.project.as_mut()) {
                    if let Some(a) = p.annotations.get_mut(i) {
                        a.x = (n.0 - ox).clamp(-0.5, 1.0);
                        a.y = (n.1 - oy).clamp(-0.5, 1.0);
                    } else if let (Some(start), Some(z)) =
                        (self.drag_start, p.zooms.get_mut(usize::MAX - i))
                    {
                        z.rect = NRect::from_points(start, n);
                    }
                }
            }
            // Rubber band while drawing a new item: a red line for arrows, a box otherwise.
            if let (Some(a), Some(b)) = (self.drag_start, self.drag_now)
                && self.moving.is_none()
            {
                let r = Rect::from_two_pos(
                    img_rect.min + Vec2::new(a.0 * size.x, a.1 * size.y),
                    img_rect.min + Vec2::new(b.0 * size.x, b.1 * size.y),
                );
                if self.tool == Tool::Add(AnnotationKind::Arrow) {
                    painter.line_segment(
                        [
                            img_rect.min + Vec2::new(a.0 * size.x, a.1 * size.y),
                            img_rect.min + Vec2::new(b.0 * size.x, b.1 * size.y),
                        ],
                        Stroke::new(3.0, Color32::from_rgb(229, 57, 53)),
                    );
                } else {
                    painter.rect_stroke(
                        r,
                        0.0,
                        Stroke::new(1.5, Color32::from_rgb(255, 214, 0)),
                        StrokeKind::Outside,
                    );
                }
            }
            // Releasing a rubber band creates the item.
            if resp.drag_stopped() {
                if let (Some(a), Some(b)) = (self.drag_start, self.drag_now)
                    && self.moving.is_none()
                {
                    self.finish_draw(a, b);
                }
                self.drag_start = None;
                self.drag_now = None;
                self.moving = None;
            }
            // Click: select (or deselect) with the select tool; text, callouts and step numbers
            // can also be placed with a click, at a default size.
            if resp.clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let n = to_norm(pos);
                match self.tool {
                    Tool::Select => {
                        self.selected = self
                            .hit_annotation(n)
                            .map(Selected::Annotation)
                            .unwrap_or(Selected::None);
                    }
                    Tool::Add(kind)
                        if matches!(
                            kind,
                            AnnotationKind::Text | AnnotationKind::Step | AnnotationKind::Callout
                        ) =>
                    {
                        // Steps get a box that is square in pixels.
                        let b = (n.0 + 0.2, n.1 + 0.1);
                        let b = if kind == AnnotationKind::Step {
                            (n.0 + 0.05, n.1 + 0.05 * vw / vh)
                        } else {
                            b
                        };
                        self.finish_draw(n, b);
                    }
                    _ => {}
                }
            }
        }

        /// Returns the topmost annotation visible at the playhead whose box contains the
        /// normalised point `n`.
        ///
        /// Boxes get a small margin and a minimum size so that thin items such as horizontal
        /// arrows can still be hit.
        fn hit_annotation(&self, n: (f32, f32)) -> Option<usize> {
            let p = self.project.as_ref()?;
            p.annotations
                .iter()
                .enumerate()
                .rev()
                .find(|(_, a)| {
                    a.span.contains(self.time) && {
                        let r = a.rect();
                        let pad = 0.01;
                        n.0 >= r.x - pad
                            && n.0 <= r.x + r.w.max(0.02) + pad
                            && n.1 >= r.y - pad
                            && n.1 <= r.y + r.h.max(0.02) + pad
                    }
                })
                .map(|(i, _)| i)
        }

        /// Creates a zoom or annotation (depending on the tool) from a drag from `a` to `b` in
        /// normalised frame coordinates, selects it and returns to the select tool.
        ///
        /// The new item's time span is the timeline selection if the playhead is in it (or at
        /// its start); otherwise 3 s from the playhead, or the last 3 s when the playhead is at
        /// the end. Tiny drags are ignored, except for kinds that can be placed with a click.
        fn finish_draw(&mut self, a: (f32, f32), b: (f32, f32)) {
            let dur = self.duration();
            let time = self.time;
            let sel = self.sel;
            let Some(p) = self.project.as_mut() else {
                return;
            };
            let span = sel
                .filter(|s| s.contains(time) || (s.start - time).abs() < 0.05)
                .unwrap_or_else(|| Span::new(time, (time + 3.0).min(dur)));
            let span = if span.len() < 0.1 {
                Span::new((dur - 3.0).max(0.0), dur)
            } else {
                span
            };
            let tiny = (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01;
            match self.tool {
                Tool::Zoom => {
                    if !tiny {
                        p.zooms.push(Zoom {
                            span,
                            rect: NRect::from_points(a, b),
                        });
                        self.selected = Selected::Zoom(p.zooms.len() - 1);
                        self.tool = Tool::Select;
                    }
                }
                Tool::Add(kind) => {
                    if tiny
                        && !matches!(
                            kind,
                            AnnotationKind::Text | AnnotationKind::Step | AnnotationKind::Callout
                        )
                    {
                        return;
                    }
                    // Step numbers count up automatically.
                    let mut ann = Annotation::new(kind, span, a, b);
                    if kind == AnnotationKind::Step {
                        ann.number = p
                            .annotations
                            .iter()
                            .filter(|x| x.kind == AnnotationKind::Step)
                            .count() as u32
                            + 1;
                    }
                    p.annotations.push(ann);
                    self.selected = Selected::Annotation(p.annotations.len() - 1);
                    self.tool = Tool::Select;
                }
                Tool::Select => {}
            }
        }

        /// Draws the annotations visible at the playhead over the video in `img_rect`.
        ///
        /// They are rendered with the export renderer ([`export::render_annotation`]) so the
        /// preview matches the exported video. The result is cached in a texture and only
        /// re-rendered when the project, the set of visible annotations or the size changes.
        /// Blur areas are shown as grey placeholders (the real blur needs the video pixels), and
        /// the selected annotation gets a dashed outline.
        fn paint_annotations(
            &mut self,
            ctx: &egui::Context,
            painter: &egui::Painter,
            img_rect: Rect,
        ) {
            let Some(p) = &self.project else { return };
            let active: Vec<usize> = p
                .annotations
                .iter()
                .enumerate()
                .filter(|(_, a)| a.span.contains(self.time))
                .map(|(i, _)| i)
                .collect();
            // Render at the physical pixel size of the preview, capped at 1080p.
            let ppp = ctx.pixels_per_point();
            let w = ((img_rect.width() * ppp) as u32).clamp(16, 1920);
            let h = ((img_rect.height() * ppp) as u32).clamp(16, 1080);
            let key = format!("{}-{:?}-{w}x{h}", self.rev, active);
            let need = self
                .overlay_tex
                .as_ref()
                .map(|(_, k)| *k != key)
                .unwrap_or(true);
            // Composite all visible annotations into one pixmap and upload it.
            if need && let Some(mut pm) = tiny_skia::Pixmap::new(w, h) {
                for &i in &active {
                    if let Some(a_pm) =
                        export::render_annotation(&p.annotations[i], w, h, &self.font)
                    {
                        pm.draw_pixmap(
                            0,
                            0,
                            a_pm.as_ref(),
                            &tiny_skia::PixmapPaint::default(),
                            tiny_skia::Transform::identity(),
                            None,
                        );
                    }
                }
                let data = pm.take_demultiplied();
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &data);
                match &mut self.overlay_tex {
                    Some((t, k)) => {
                        t.set(img, egui::TextureOptions::LINEAR);
                        *k = key;
                    }
                    None => {
                        self.overlay_tex = Some((
                            ctx.load_texture("editor-overlay", img, egui::TextureOptions::LINEAR),
                            key,
                        ));
                    }
                }
            }
            if let Some((tex, _)) = &self.overlay_tex {
                painter.image(
                    tex.id(),
                    img_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            // Blur regions and selection outlines.
            for &i in &active {
                let a = &p.annotations[i];
                let r = a.rect();
                let rr = Rect::from_min_size(
                    img_rect.min + Vec2::new(r.x * img_rect.width(), r.y * img_rect.height()),
                    Vec2::new(r.w * img_rect.width(), r.h * img_rect.height()),
                );
                if a.kind == AnnotationKind::Blur {
                    painter.rect_filled(
                        rr,
                        0.0,
                        Color32::from_rgba_unmultiplied(128, 128, 128, 170),
                    );
                    painter.text(
                        rr.center(),
                        Align2::CENTER_CENTER,
                        tr("Blur"),
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                }
                if self.selected == Selected::Annotation(i) {
                    painter.add(Shape::dashed_line(
                        &[
                            rr.left_top(),
                            rr.right_top(),
                            rr.right_bottom(),
                            rr.left_bottom(),
                            rr.left_top(),
                        ],
                        Stroke::new(1.0, Color32::WHITE),
                        5.0,
                        4.0,
                    ));
                }
            }
        }

        /// Draws the properties panel on the right.
        ///
        /// From top to bottom: the selected item's properties (or usage hints), clip settings
        /// (trim, speed, volume, fades), pause removal, background music and export.
        fn side_panel(&mut self, ui: &mut egui::Ui) {
            let dur = self.duration();
            let time = self.time;
            ui.add_space(6.0);
            ui.heading(tr("Properties"));
            ui.separator();
            let Some(p) = self.project.as_mut() else {
                return;
            };
            // Seeking and deleting need `&mut self`, which is borrowed by `p` below, so they are
            // recorded here and done after the match.
            let mut delete = false;
            let mut seek_to = None;
            match self.selected {
                Selected::Annotation(i) if i < p.annotations.len() => {
                    let a = &mut p.annotations[i];
                    ui.strong(a.kind.label());
                    span_editor(ui, &mut a.span, dur, time);
                    if a.kind.has_text() {
                        ui.label(tr("Text:"));
                        ui.text_edit_multiline(&mut a.text);
                    }
                    if a.kind != AnnotationKind::Blur {
                        ui.horizontal(|ui| {
                            ui.label(tr("Colour:"));
                            ui.color_edit_button_srgba_unmultiplied(&mut a.color);
                        });
                    }
                    // Sizes are stored as fractions of the frame height but edited as percentages.
                    if matches!(
                        a.kind,
                        AnnotationKind::Text | AnnotationKind::Callout | AnnotationKind::Step
                    ) {
                        let mut pct = a.size * 100.0;
                        ui.add(egui::Slider::new(&mut pct, 1.5..=20.0).text(tr("Size %")));
                        a.size = pct / 100.0;
                    }
                    if matches!(
                        a.kind,
                        AnnotationKind::Rectangle | AnnotationKind::Ellipse | AnnotationKind::Arrow
                    ) {
                        let mut pct = a.stroke * 100.0;
                        ui.add(egui::Slider::new(&mut pct, 0.1..=3.0).text(tr("Line width %")));
                        a.stroke = pct / 100.0;
                    }
                    if a.kind == AnnotationKind::Step {
                        ui.add(
                            egui::DragValue::new(&mut a.number)
                                .range(0..=999)
                                .prefix(tr("Number: ")),
                        );
                    }
                    if ui.button(tr("Jump to start")).clicked() {
                        seek_to = Some(a.span.start);
                    }
                    delete = ui.button(tr("🗑 Delete annotation")).clicked();
                }
                Selected::Zoom(i) if i < p.zooms.len() => {
                    let z = &mut p.zooms[i];
                    ui.strong(tr("Zoom effect"));
                    span_editor(ui, &mut z.span, dur, time);
                    ui.label(tr(
                        "Drag on the video (Select tool) to change the zoom area.",
                    ));
                    ui.add(egui::Slider::new(&mut z.rect.x, 0.0..=0.95).text(tr("Left")));
                    ui.add(egui::Slider::new(&mut z.rect.y, 0.0..=0.95).text(tr("Top")));
                    ui.add(egui::Slider::new(&mut z.rect.w, 0.05..=1.0).text(tr("Width")));
                    ui.add(egui::Slider::new(&mut z.rect.h, 0.05..=1.0).text(tr("Height")));
                    if ui.button(tr("Match video aspect ratio")).clicked() {
                        // Equal normalised width and height keep the video's aspect ratio; the
                        // area is also kept inside the frame.
                        let side = z.rect.w.min(z.rect.h).max(0.05);
                        z.rect.w = side.min(1.0 - z.rect.x);
                        z.rect.h = side.min(1.0 - z.rect.y);
                    }
                    delete = ui.button(tr("🗑 Delete zoom")).clicked();
                }
                Selected::Title(i) if i < p.titles.len() => {
                    let t = &mut p.titles[i];
                    ui.strong(tr("Title page"));
                    ui.add(
                        egui::DragValue::new(&mut t.at)
                            .range(0.0..=dur)
                            .speed(0.05)
                            .prefix(tr("Insert at: "))
                            .suffix(" s"),
                    );
                    ui.add(
                        egui::DragValue::new(&mut t.duration)
                            .range(0.5..=30.0)
                            .speed(0.05)
                            .prefix(tr("Duration: "))
                            .suffix(" s"),
                    );
                    ui.label(tr("Title:"));
                    ui.text_edit_multiline(&mut t.title);
                    ui.label(tr("Subtitle:"));
                    ui.text_edit_singleline(&mut t.subtitle);
                    ui.horizontal(|ui| {
                        ui.label(tr("Background:"));
                        ui.color_edit_button_srgba_unmultiplied(&mut t.background);
                        ui.label(tr("Text:"));
                        ui.color_edit_button_srgba_unmultiplied(&mut t.foreground);
                    });
                    delete = ui.button(tr("🗑 Delete title page")).clicked();
                }
                Selected::Cut(i) if i < p.cuts.len() => {
                    ui.strong(tr("Removed section"));
                    span_editor(ui, &mut p.cuts[i], dur, time);
                    delete = ui.button(tr("↩ Restore section")).clicked();
                }
                Selected::SpeedUp(i) if i < p.speedups.len() => {
                    ui.strong(tr("Faster section"));
                    span_editor(ui, &mut p.speedups[i].span, dur, time);
                    ui.horizontal(|ui| {
                        ui.label(tr("Speed:"));
                        for f in [2.0f32, 4.0, 8.0] {
                            ui.radio_value(&mut p.speedups[i].factor, f, format!("{}×", f as u32));
                        }
                    });
                    ui.label(
                        egui::RichText::new(tr(
                            "Plays faster and muted in the export, with a fast-forward badge.",
                        ))
                        .small()
                        .weak(),
                    );
                    delete = ui.button(tr("🗑 Normal speed")).clicked();
                }
                // Nothing selected: short usage hints.
                _ => {
                    ui.label(tr("Select an item on the video or the timeline."));
                    ui.label(tr(
                        "• Drag on the timeline to scrub; drag the yellow handles to trim.",
                    ));
                    ui.label(tr(
                        "• Drag on the ruler to select a range, then Cut or Zoom it.",
                    ));
                    ui.label(tr("• Pick a drawing tool and drag on the video."));
                }
            }
            // Markers dropped while recording: one button per marker jumps to it.
            if !self.markers.is_empty() {
                ui.add_space(10.0);
                ui.heading(tr("Markers"));
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    for m in &self.markers {
                        if ui.button(format!("🚩 {}", format_time(*m))).clicked() {
                            seek_to = Some(*m);
                        }
                    }
                });
            }
            if let Some(t) = seek_to {
                self.seek(t);
            }
            if delete {
                self.delete_selected();
            }
            // Borrow the project again after the deferred actions.
            let Some(p) = self.project.as_mut() else {
                return;
            };

            ui.add_space(10.0);
            ui.heading(tr("Clip"));
            ui.separator();
            let trim = p.trim_span(dur);
            ui.label(tf(
                "Trim: {} – {}",
                &[&format_time(trim.start), &format_time(trim.end)],
            ));
            if p.trim.is_some() && ui.button(tr("Reset trim")).clicked() {
                p.trim = None;
            }
            ui.add(
                egui::Slider::new(&mut p.speed, 0.25..=4.0)
                    .text(tr("Speed ×"))
                    .logarithmic(true),
            );
            ui.add(egui::Slider::new(&mut p.volume, 0.0..=3.0).text(tr("Volume")));
            ui.checkbox(&mut p.mute, tr("Mute audio"));
            ui.add(egui::Slider::new(&mut p.fade_in, 0.0..=5.0).text(tr("Fade in (s)")));
            ui.add(egui::Slider::new(&mut p.fade_out, 0.0..=5.0).text(tr("Fade out (s)")));
            // Length of the exported video, including title pages and the speed change.
            let out_dur = p.output_duration(dur) / p.speed.max(0.01) as f64;
            ui.label(tf("Output length: {}", &[&format_time(out_dur)]));

            // Pause removal: find quiet stretches in the audio and offer to cut them.
            ui.add_space(10.0);
            ui.heading(tr("Pauses"));
            ui.separator();
            let a = ui.add(
                egui::Slider::new(&mut self.pause_min, 0.3..=3.0).text(tr("Shortest pause (s)")),
            );
            let b = ui.add(
                egui::Slider::new(&mut self.pause_keep, 0.0..=0.5)
                    .text(tr("Keep next to speech (s)")),
            );
            // A result found with other settings is no longer valid.
            if a.changed() || b.changed() {
                self.pause_found = None;
            }
            // Poll a running search (showing a spinner), or offer to start one.
            if let Some(rx) = &self.pause_search {
                match rx.try_recv() {
                    Ok(r) => {
                        // Leave out pauses inside parts already cut.
                        self.pause_found = Some(r.map(|v| {
                            v.into_iter()
                                .filter(|(a, b)| !p.is_cut((a + b) / 2.0, dur))
                                .collect()
                        }));
                        self.pause_search = None;
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tr("Looking for pauses…"));
                        });
                    }
                    // The search thread ended without a result.
                    Err(_) => self.pause_search = None,
                }
            } else if ui
                .button(tr("🔍 Find pauses"))
                .on_hover_text(tr("Find quiet parts in the audio that can be cut"))
                .clicked()
            {
                // Decode the whole audio track and search it on a background thread; the UI is
                // woken when the result is ready.
                let (tx, rx) = crossbeam_channel::bounded(1);
                let (path, min, keep) = (p.source.clone(), self.pause_min, self.pause_keep);
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let r = super::render::decode_audio(&path, &Job::new("pauses"))
                        .map(|audio| crate::media::voice::find_pauses(&audio, min, keep))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(r);
                    ctx.request_repaint();
                });
                self.pause_search = Some(rx);
                self.pause_found = None;
            }
            match &self.pause_found {
                Some(Ok(found)) if found.is_empty() => {
                    ui.label(tr("No pauses found."));
                }
                Some(Ok(found)) => {
                    let total: f64 = found.iter().map(|(a, b)| b - a).sum();
                    ui.label(tf(
                        "Found {} pauses ({} in total)",
                        &[&found.len(), &format_time(total)],
                    ));
                    if ui
                        .button(tr("✂ Cut pauses"))
                        .on_hover_text(tr("Cut them; they show on the timeline and can be undone"))
                        .clicked()
                    {
                        p.cuts
                            .extend(found.iter().map(|(a, b)| project::Span::new(*a, *b)));
                        self.pause_found = None;
                    }
                }
                Some(Err(e)) => {
                    ui.colored_label(crate::util::error_color(ui), e.as_str());
                }
                None => {}
            }

            // Background music: add a file, or adjust or remove the current one.
            ui.add_space(10.0);
            ui.heading(tr("Music"));
            ui.separator();
            // Removal is deferred because `p.music` is borrowed by the match.
            let mut remove = false;
            match &mut p.music {
                None => {
                    if ui
                        .button(tr("🎵 Add music…"))
                        .on_hover_text(tr("Play music under the whole video"))
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter(tr("Audio"), &["mp3", "m4a", "aac", "wav", "wma"])
                            .pick_file()
                    {
                        p.music = Some(project::Music {
                            path,
                            ..Default::default()
                        });
                    }
                }
                Some(m) => {
                    let name = m
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    ui.label(egui::RichText::new(format!("🎵 {name}")).strong());
                    ui.add(egui::Slider::new(&mut m.volume, 0.0..=1.5).text(tr("Music volume")));
                    ui.checkbox(&mut m.duck, tr("Lower the music while someone speaks"));
                    if ui.button(tr("Remove music")).clicked() {
                        remove = true;
                    }
                    if self.export_opts.format == OutputFormat::Gif {
                        ui.label(egui::RichText::new(tr("GIFs have no sound.")).weak());
                    }
                }
            }
            if remove {
                p.music = None;
            }

            // Logo in a corner of the exported video.
            ui.add_space(10.0);
            ui.heading(tr("Logo"));
            ui.separator();
            let mut remove_logo = false;
            match &mut p.watermark {
                None => {
                    if ui
                        .button(tr("🏷 Add logo…"))
                        .on_hover_text(tr("Show your logo in a corner of the exported video"))
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter(tr("Image"), &["png", "jpg", "jpeg"])
                            .pick_file()
                    {
                        p.watermark = Some(project::Watermark {
                            path,
                            ..Default::default()
                        });
                    }
                }
                Some(wm) => {
                    let name = wm
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    ui.label(egui::RichText::new(format!("🏷 {name}")).strong());
                    egui::ComboBox::from_label(tr("Corner"))
                        .selected_text(wm.corner.label())
                        .show_ui(ui, |ui| {
                            for c in crate::settings::WebcamPosition::ALL {
                                ui.selectable_value(&mut wm.corner, c, c.label());
                            }
                        });
                    let mut pct = (wm.size * 100.0).round();
                    ui.add(
                        egui::Slider::new(&mut pct, 5.0..=40.0).text(tr("Logo size (% of width)")),
                    );
                    wm.size = pct / 100.0;
                    let mut op = (wm.opacity * 100.0).round();
                    ui.add(egui::Slider::new(&mut op, 10.0..=100.0).text(tr("Opacity (%)")));
                    wm.opacity = op / 100.0;
                    if ui.button(tr("Remove logo")).clicked() {
                        remove_logo = true;
                    }
                }
            }
            if remove_logo {
                p.watermark = None;
            }

            // Export settings, the export button and the status of the last export.
            ui.add_space(10.0);
            ui.heading(tr("Export"));
            ui.separator();
            egui::ComboBox::from_label(tr("Format"))
                .selected_text(self.export_opts.format.label())
                .show_ui(ui, |ui| {
                    for f in [OutputFormat::Mp4, OutputFormat::Gif] {
                        ui.selectable_value(&mut self.export_opts.format, f, f.label());
                    }
                });
            egui::ComboBox::from_label(tr("Shape"))
                .selected_text(self.export_opts.shape.label())
                .show_ui(ui, |ui| {
                    for sh in export::Shape::ALL {
                        ui.selectable_value(&mut self.export_opts.shape, sh, sh.label());
                    }
                });
            if self.export_opts.shape != export::Shape::Original {
                ui.label(
                    egui::RichText::new(tr(
                        "The frame follows the mouse pointer, as shown on the video.",
                    ))
                    .small()
                    .weak(),
                );
            }
            if self.export_opts.format == OutputFormat::Gif {
                ui.add(
                    egui::Slider::new(&mut self.export_opts.gif_fps, 5..=30).text(tr("GIF fps")),
                );
                ui.add(
                    egui::Slider::new(&mut self.export_opts.gif_width, 160..=1920)
                        .text(tr("GIF max width")),
                );
            } else {
                ui.add(
                    egui::Slider::new(&mut self.export_opts.quality, 1..=100).text(tr("Quality")),
                );
                ui.add(
                    egui::Slider::new(&mut self.export_opts.scale_percent, 25..=100)
                        .text(tr("Scale %")),
                );
            }

            // Only one export runs at a time.
            let busy = self.job.as_ref().is_some_and(|j| !j.is_finished());
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button(tr("💾 Export video…")).clicked() {
                    self.start_export();
                }
            });
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
                        if ui.button(tr("Show in folder")).clicked() {
                            crate::util::reveal_in_folder(&output);
                        }
                    }
                    JobStatus::Failed(e) => {
                        ui.colored_label(crate::util::error_color(ui), e);
                    }
                }
            }
            if let Some(m) = &self.message {
                ui.colored_label(crate::util::error_color(ui), m);
            }
        }

        /// Asks for an output file and starts exporting the project as a background [`Job`].
        ///
        /// The dialog suggests `<name>_edited.<ext>` next to the source; the extension is added
        /// if the user leaves it out. Overwriting the source video is refused, because it is
        /// still being read during the export. The project and settings are copied, so editing
        /// can continue meanwhile.
        fn start_export(&mut self) {
            let Some(p) = self.project.clone() else {
                return;
            };
            let ext = self.export_opts.format.extension();
            let stem = p
                .source
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "video".into());
            let dir = p.source.parent().map(Path::to_path_buf).unwrap_or_default();
            let Some(out) = rfd::FileDialog::new()
                .set_directory(&dir)
                .set_file_name(format!("{stem}_edited.{ext}"))
                .add_filter(ext.to_uppercase(), &[ext])
                .save_file()
            else {
                return;
            };
            let out: PathBuf = if out.extension().is_none() {
                out.with_extension(ext)
            } else {
                out
            };
            if out == p.source {
                self.message =
                    Some(tr("Please choose a different file name than the source video.").into());
                return;
            }
            self.message = None;
            let opts = self.export_opts.clone();
            let font = self.font.clone();
            self.job = Some(Job::spawn("Export", move |job| {
                export::export(&p, &opts, &out, &font, job)?;
                Ok((out, tr("Export finished").into()))
            }));
        }
    }

    /// Draws editors for a time span: start and end fields (0..=`dur` seconds) and buttons that
    /// set either end to the playhead time `now`. The span is kept ordered.
    fn span_editor(ui: &mut egui::Ui, span: &mut Span, dur: f64, now: f64) {
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut span.start)
                    .range(0.0..=dur)
                    .speed(0.05)
                    .prefix(tr("From "))
                    .suffix(" s")
                    .max_decimals(2),
            );
            ui.add(
                egui::DragValue::new(&mut span.end)
                    .range(0.0..=dur)
                    .speed(0.05)
                    .prefix(tr("to "))
                    .suffix(" s")
                    .max_decimals(2),
            );
        });
        ui.horizontal(|ui| {
            if ui.small_button(tr("Start = playhead")).clicked() {
                span.start = now.min(span.end - 0.05).max(0.0);
            }
            if ui.small_button(tr("End = playhead")).clicked() {
                span.end = now.max(span.start + 0.05).min(dur);
            }
        });
        if span.end < span.start {
            std::mem::swap(&mut span.start, &mut span.end);
        }
    }
}
