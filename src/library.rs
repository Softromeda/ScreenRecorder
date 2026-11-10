// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Recordings library window.
//!
//! [`Library`] lists the videos, GIFs and audio files in the output folder, newest first, with
//! a thumbnail, length, date and size, and lets the user search, play, open in the video
//! editor, rename, show in File Explorer or delete them (to the Recycle Bin). The folder is
//! rescanned every few seconds so that new recordings appear on their own. Lengths and
//! thumbnails are read on a background worker thread, only for rows that become visible.
//! The app calls [`Library::show`] every frame and opens the editor for the file it returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use egui::{Align, Color32, CornerRadius, Layout, RichText, Sense, TextureHandle, Vec2};

#[allow(unused_imports)]
use crate::i18n::{tf, tr};
use crate::media::format_time;

/// Maximum thumbnail width in pixels; thumbnails are decoded at this size.
const THUMB_W: u32 = 192;
/// Height of one list row in points (needed up front for virtual scrolling).
const ROW_H: f32 = 92.0;

/// The kind of media file, decided by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A video container (MP4, MOV, MKV, ...); can be opened in the editor.
    Video,
    /// An animated GIF, decoded with the `gif` crate rather than Media Foundation.
    Gif,
    /// An audio-only file (MP3, M4A, WAV); has a length but no thumbnail.
    Audio,
}

impl Kind {
    /// Classifies `path` by its extension (case-insensitive); `None` for files not listed.
    fn of(path: &Path) -> Option<Kind> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "mov" | "m4v" | "wmv" | "avi" | "mkv" => Some(Kind::Video),
            "gif" => Some(Kind::Gif),
            "mp3" | "m4a" | "wav" => Some(Kind::Audio),
            _ => None,
        }
    }

    /// Returns the placeholder icon shown while there is no thumbnail.
    fn icon(self) -> &'static str {
        match self {
            Kind::Video => "🎬",
            Kind::Gif => "🖼",
            Kind::Audio => "🎵",
        }
    }
}

/// One file in the output folder, as found by the last scan.
#[derive(Debug, Clone)]
struct Item {
    /// Full path of the file; also the key for the cached details.
    path: PathBuf,
    /// File name including the extension, as displayed and searched.
    name: String,
    /// Media kind, from the extension.
    kind: Kind,
    /// Last modification time; the list is sorted by it, newest first.
    modified: SystemTime,
    /// File size in bytes.
    size: u64,
}

/// What the background worker found out about a file.
struct Details {
    /// The file these details belong to, as sent to the worker.
    path: PathBuf,
    /// Length in seconds, or `None` if unknown or zero.
    duration: Option<f64>,
    /// Thumbnail image, or `None` for audio files and unreadable files.
    thumb: Option<egui::ColorImage>,
}

/// State of the recordings library window.
pub struct Library {
    /// Whether the window is shown.
    open: bool,
    /// The folder being listed (the configured output folder).
    dir: PathBuf,
    /// Files found by the last scan, newest first.
    items: Vec<Item>,
    /// When the folder was last scanned; `None` forces a rescan on the next frame.
    scanned: Option<Instant>,
    /// Search text; rows whose name contains it (case-insensitive) are shown.
    filter: String,
    /// Lengths received from the worker; `None` when the length could not be read.
    durations: HashMap<PathBuf, Option<f64>>,
    /// Thumbnail textures received from the worker; `None` when there is no thumbnail.
    thumbs: HashMap<PathBuf, Option<TextureHandle>>,
    /// Files already sent to the worker, so that each is only examined once.
    requested: std::collections::HashSet<PathBuf>,
    /// Channels to the worker thread (paths in, details out); started on first use.
    worker: Option<(Sender<PathBuf>, Receiver<Details>)>,
    /// File being renamed, the new name, and whether the box needs focus.
    renaming: Option<(PathBuf, String, bool)>,
    /// File whose row shows the delete confirmation.
    confirm_delete: Option<PathBuf>,
    /// Status line under the toolbar: (is an error, text).
    message: Option<(bool, String)>,
}

impl Library {
    /// Creates a closed library; nothing is scanned until it is opened.
    pub fn new() -> Self {
        Self {
            open: false,
            dir: PathBuf::new(),
            items: vec![],
            scanned: None,
            filter: String::new(),
            durations: HashMap::new(),
            thumbs: HashMap::new(),
            requested: Default::default(),
            worker: None,
            renaming: None,
            confirm_delete: None,
            message: None,
        }
    }

    /// Returns whether the window is shown.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the window and rescans the folder on the next frame.
    pub fn open(&mut self) {
        self.open = true;
        self.scanned = None; // rescan
    }

    /// Reads the output folder into `items`, newest first, and drops cached details of files
    /// that are gone. A missing or unreadable folder gives an empty list.
    fn scan(&mut self) {
        // Keep regular files with a known media extension; unreadable entries are skipped.
        let mut items: Vec<Item> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let path = e.path();
                        let kind = Kind::of(&path)?;
                        let meta = e.metadata().ok()?;
                        if !meta.is_file() {
                            return None;
                        }
                        Some(Item {
                            name: path.file_name()?.to_string_lossy().to_string(),
                            path,
                            kind,
                            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                            size: meta.len(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.sort_by_key(|i| std::cmp::Reverse(i.modified));
        // Forget details of files that disappeared (or were renamed), so that a file appearing
        // again under that path is examined afresh.
        let keep: std::collections::HashSet<&PathBuf> = items.iter().map(|i| &i.path).collect();
        self.durations.retain(|p, _| keep.contains(p));
        self.thumbs.retain(|p, _| keep.contains(p));
        self.requested.retain(|p| keep.contains(p));
        self.items = items;
        self.scanned = Some(Instant::now());
    }

    /// Returns the sender for paths to examine, starting the worker thread on first use.
    ///
    /// The worker initialises Media Foundation, computes [`details`] for each path in turn and
    /// requests a repaint after each result. It ends when the library is dropped.
    fn worker(&mut self, ctx: &egui::Context) -> &Sender<PathBuf> {
        if self.worker.is_none() {
            let (tx, rx) = crossbeam_channel::unbounded::<PathBuf>();
            let (dtx, drx) = crossbeam_channel::unbounded::<Details>();
            let ctx = ctx.clone();
            let _ = std::thread::Builder::new()
                .name("library".into())
                .spawn(move || {
                    let _ = crate::media::mf::init();
                    while let Ok(path) = rx.recv() {
                        let d = details(&path);
                        if dtx.send(d).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            self.worker = Some((tx, drx));
        }
        &self.worker.as_ref().unwrap().0
    }

    /// Stores the results the worker has produced since the last frame, uploading thumbnails
    /// as textures.
    fn receive(&mut self, ctx: &egui::Context) {
        let Some((_, rx)) = &self.worker else { return };
        while let Ok(d) = rx.try_recv() {
            let tex = d.thumb.map(|img| {
                ctx.load_texture(
                    format!("thumb:{}", d.path.display()),
                    img,
                    egui::TextureOptions::LINEAR,
                )
            });
            self.durations.insert(d.path.clone(), d.duration);
            self.thumbs.insert(d.path, tex);
        }
    }

    /// Shows the window while it is open; call every frame with the current output folder.
    ///
    /// Returns the path of a video the user chose to open in the editor, if any.
    pub fn show(&mut self, ctx: &egui::Context, dir: &Path) -> Option<PathBuf> {
        if !self.open {
            return None;
        }
        // A changed output folder is rescanned straight away.
        if self.dir != dir {
            self.dir = dir.to_path_buf();
            self.scanned = None;
        }
        // Rescan every 3 seconds so that new recordings appear on their own.
        if self
            .scanned
            .is_none_or(|t| t.elapsed() > Duration::from_secs(3))
        {
            self.scan();
        }
        self.receive(ctx);
        let mut edit = None;
        let mut close = false;
        // Separate native window, created hidden and revealed once laid out.
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("library"),
            crate::geometry::centered(
                egui::ViewportBuilder::default()
                    .with_title(tr("Recordings"))
                    .with_inner_size([860.0, 620.0])
                    .with_min_inner_size([560.0, 360.0])
                    .with_visible(false),
                [860.0, 620.0],
            ),
            |ui, _| {
                crate::util::reveal_when_ready(ui.ctx());
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::Panel::top("library-top").show(ui, |ui| {
                    ui.add_space(6.0);
                    self.toolbar(ui);
                    ui.add_space(6.0);
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    edit = self.list(ui);
                });
                // Wake up periodically for the rescan even without user input.
                ui.ctx().request_repaint_after(Duration::from_secs(3));
            },
        );
        // Closing abandons any rename or delete confirmation in progress.
        if close {
            self.open = false;
            self.renaming = None;
            self.confirm_delete = None;
            crate::util::forget_reveal(ctx, egui::ViewportId::from_hash_of("library"));
        }
        edit
    }

    /// Draws the toolbar (search, refresh, open folder, totals) and the status message.
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(tr("🔍 Search"))
                    .desired_width(220.0),
            );
            if ui.button(tr("⟳ Refresh")).clicked() {
                self.scan();
            }
            // Create the folder first so that Explorer opens it even before the first recording.
            if ui.button(tr("📁 Output folder")).clicked() {
                let _ = std::fs::create_dir_all(&self.dir);
                crate::util::open_path(&self.dir);
            }
            // Number of files and their total size, right-aligned.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let total: u64 = self.items.iter().map(|i| i.size).sum();
                ui.label(
                    RichText::new(tf(
                        "{} recordings · {}",
                        &[&self.items.len(), &format_size(total)],
                    ))
                    .weak(),
                );
            });
        });
        if let Some((err, m)) = &self.message {
            let color = if *err {
                crate::util::error_color(ui)
            } else {
                crate::util::ok_color(ui)
            };
            ui.colored_label(color, m.as_str());
        }
    }

    /// Draws the (filtered) list of files and returns a video to edit, if one was chosen.
    ///
    /// Only visible rows are laid out, and details are requested from the worker as rows come
    /// into view, so large folders stay fast.
    fn list(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let filter = self.filter.to_lowercase();
        // Cloned so that rows can mutate `self` while iterating.
        let items: Vec<Item> = self
            .items
            .iter()
            .filter(|i| filter.is_empty() || i.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        // Empty state: distinguish an empty folder from a search without matches.
        if items.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(if self.items.is_empty() {
                        tf("No recordings yet in {}", &[&self.dir.display()])
                    } else {
                        tr("Nothing matches the search.").to_string()
                    })
                    .weak(),
                );
            });
            return None;
        }
        let mut edit = None;
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, items.len(), |ui, range| {
                for item in &items[range] {
                    if !self.requested.contains(&item.path) {
                        self.requested.insert(item.path.clone());
                        let _ = self.worker(&ctx).send(item.path.clone());
                    }
                    if let Some(p) = self.row(ui, item) {
                        edit = Some(p);
                    }
                }
            });
        edit
    }

    /// Draws one row: thumbnail (click to play), name or rename box, date and size, and the
    /// action buttons or the delete confirmation. Returns the path if Edit was clicked.
    fn row(&mut self, ui: &mut egui::Ui, item: &Item) -> Option<PathBuf> {
        let mut edit = None;
        // Reserve the full row height and lay out the contents in a child UI inside it.
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(4.0, 6.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let ui = &mut child;
        // Thumbnail, fitted into a 16:9 box without distortion, or the kind's icon while no
        // thumbnail is available.
        let thumb_size = Vec2::new(128.0, 72.0);
        let (trect, tresp) = ui.allocate_exact_size(thumb_size, Sense::click());
        let painter = ui.painter();
        painter.rect_filled(trect, CornerRadius::same(6), Color32::from_gray(30));
        match self.thumbs.get(&item.path) {
            Some(Some(tex)) => {
                let size = tex.size_vec2();
                let fit = (thumb_size.x / size.x).min(thumb_size.y / size.y);
                let r = egui::Rect::from_center_size(trect.center(), size * fit);
                painter.image(
                    tex.id(),
                    r,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            _ => {
                painter.text(
                    trect.center(),
                    egui::Align2::CENTER_CENTER,
                    item.kind.icon(),
                    egui::FontId::proportional(26.0),
                    Color32::from_gray(170),
                );
            }
        }
        // Length badge in the bottom-right corner, rounded up to whole seconds and shown
        // without the fractional part.
        if let Some(Some(d)) = self.durations.get(&item.path) {
            let text = format_time(d.ceil());
            let text = text.split('.').next().unwrap_or(&text).to_string();
            let galley =
                painter.layout_no_wrap(text, egui::FontId::monospace(11.0), Color32::WHITE);
            let r = egui::Rect::from_min_size(
                trect.right_bottom() - galley.size() - Vec2::new(10.0, 6.0),
                galley.size() + Vec2::new(6.0, 2.0),
            );
            painter.rect_filled(r, CornerRadius::same(3), Color32::from_black_alpha(190));
            painter.galley(r.min + Vec2::new(3.0, 1.0), galley, Color32::WHITE);
        }
        if tresp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tr("Play"))
            .clicked()
        {
            crate::util::open_path(&item.path);
        }
        ui.add_space(10.0);

        // Name (or the rename box), date and size; about 330 points are left for the buttons.
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 330.0).max(160.0));
            ui.add_space(8.0);
            let renaming = self
                .renaming
                .as_ref()
                .is_some_and(|(p, _, _)| p == &item.path);
            if renaming {
                let mut finished = false;
                let mut cancel = false;
                if let Some((_, text, focus)) = &mut self.renaming {
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                    // Focus the box once, in the frame after Rename was clicked.
                    if std::mem::take(focus) {
                        r.request_focus();
                    }
                    // Enter or clicking elsewhere renames; Esc cancels.
                    finished = r.lost_focus();
                    cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                }
                if cancel {
                    self.renaming = None;
                } else if finished {
                    self.finish_rename();
                }
            } else {
                ui.add(egui::Label::new(RichText::new(&item.name).strong()).truncate());
            }
            let date = chrono::DateTime::<chrono::Local>::from(item.modified)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            ui.label(RichText::new(format!("{date}   ·   {}", format_size(item.size))).weak());
        });

        // Action buttons, laid out from the right edge (so the code order is reversed).
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // While confirming a delete, the row shows only the question and its two buttons.
            if self.confirm_delete.as_ref() == Some(&item.path) {
                if ui.button(tr("Cancel")).clicked() {
                    self.confirm_delete = None;
                }
                let del = egui::Button::new(RichText::new(tr("Delete")).color(Color32::WHITE))
                    .fill(crate::theme::RECORD_RED);
                if ui.add(del).clicked() {
                    self.delete(&item.path);
                }
                ui.label(tr("Move to the Recycle Bin?"));
                return;
            }
            if ui.button("🗑").on_hover_text(tr("Delete")).clicked() {
                self.confirm_delete = Some(item.path.clone());
            }
            // Rename edits the name without its extension, which is kept.
            if ui.button("✏").on_hover_text(tr("Rename")).clicked() {
                let stem = item
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.renaming = Some((item.path.clone(), stem, true));
            }
            if ui
                .button("📂")
                .on_hover_text(tr("Show in folder"))
                .clicked()
            {
                crate::util::reveal_in_folder(&item.path);
            }
            // Only videos can be opened in the editor.
            if item.kind == Kind::Video
                && ui
                    .button(tr("🎬 Edit"))
                    .on_hover_text(tr("Open in the video editor"))
                    .clicked()
            {
                edit = Some(item.path.clone());
            }
            if ui.button(tr("▶ Play")).clicked() {
                crate::util::open_path(&item.path);
            }
        });
        // Separator line along the bottom of the row.
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            ui.visuals().widgets.noninteractive.bg_stroke,
        );
        edit
    }

    /// Applies the rename typed in the rename box, keeping the original extension.
    ///
    /// Rejects empty names, names with characters Windows does not allow and names that are
    /// already taken, reporting the problem in the status line. The editor project and the
    /// click and pointer logs saved next to the recording are renamed along with it.
    fn finish_rename(&mut self) {
        let Some((path, name, _)) = self.renaming.take() else {
            return;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            self.message = Some((true, tr("That file name is not allowed.").to_string()));
            return;
        }
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let target = path.with_file_name(format!("{name}{ext}"));
        if target == path {
            return;
        }
        if target.exists() {
            self.message = Some((
                true,
                tr("A file with that name already exists.").to_string(),
            ));
            return;
        }
        match std::fs::rename(&path, &target) {
            Ok(()) => {
                // Move the side files too so that the editor still finds them; they are
                // optional, so failures are ignored.
                let project = |p: &Path| {
                    let mut s = p.as_os_str().to_owned();
                    s.push(".srproj.json");
                    PathBuf::from(s)
                };
                let _ = std::fs::rename(project(&path), project(&target));
                use crate::editor::project::{clicks_file, mouse_file};
                let _ = std::fs::rename(clicks_file(&path), clicks_file(&target));
                let _ = std::fs::rename(mouse_file(&path), mouse_file(&target));
                let _ = std::fs::rename(
                    crate::editor::project::markers_file(&path),
                    crate::editor::project::markers_file(&target),
                );
                self.message = None;
                self.scan();
            }
            Err(e) => self.message = Some((true, e.to_string())),
        }
    }

    /// Moves `path` to the Recycle Bin together with its editor project and click and pointer
    /// logs, then rescans. Errors are shown in the status line.
    fn delete(&mut self, path: &Path) {
        self.confirm_delete = None;
        match move_to_recycle_bin(path) {
            Ok(()) => {
                // Side files are removed only once the recording itself is gone.
                let mut s = path.as_os_str().to_owned();
                s.push(".srproj.json");
                let _ = move_to_recycle_bin(Path::new(&s));
                let _ = move_to_recycle_bin(&crate::editor::project::clicks_file(path));
                let _ = move_to_recycle_bin(&crate::editor::project::mouse_file(path));
                let _ = move_to_recycle_bin(&crate::editor::project::markers_file(path));
                self.message = None;
                self.scan();
            }
            Err(e) => self.message = Some((true, format!("{e:#}"))),
        }
    }
}

/// Formats a byte count as KB, MB or GB (binary units); sizes below 1 KB show as 1 KB.
fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", (b / 1024.0).max(1.0))
    }
}

/// Reads the length and a thumbnail of a file; runs on the worker thread.
///
/// GIFs are decoded directly; videos and audio files are probed with Media Foundation, and
/// videos also get a thumbnail. Failures give `None` values rather than an error.
fn details(path: &Path) -> Details {
    let (duration, thumb) = match Kind::of(path) {
        Some(Kind::Gif) => gif_details(path).unwrap_or((None, None)),
        Some(Kind::Video) => {
            let duration = crate::media::mf::probe(path).ok().map(|i| i.duration);
            (duration, video_thumb(path, duration.unwrap_or(0.0)))
        }
        _ => (crate::media::mf::probe(path).ok().map(|i| i.duration), None),
    };
    Details {
        path: path.to_path_buf(),
        duration: duration.filter(|d| *d > 0.0),
        thumb,
    }
}

/// Decodes a thumbnail from a video, `duration` seconds long.
///
/// The frame is taken at 10 % of the length but no later than 1 second, which skips a black
/// first frame while staying cheap to seek to. Returns `None` if the video cannot be read.
fn video_thumb(path: &Path, duration: f64) -> Option<egui::ColorImage> {
    let mut r = crate::media::mf::VideoReader::open_fast(path).ok()?;
    r.seek((duration * 0.1).min(1.0)).ok()?;
    let f = r.read_frame().ok()??;
    Some(small(&f.bgra, f.width, f.height, true))
}

/// Reads the total length in seconds and a thumbnail of the first frame of a GIF.
///
/// Returns `None` if the file cannot be decoded; the length is `None` when the GIF has no
/// frame delays.
fn gif_details(path: &Path) -> Option<(Option<f64>, Option<egui::ColorImage>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut dec = opts.read_info(std::io::BufReader::new(file)).ok()?;
    let (w, h) = (dec.width() as u32, dec.height() as u32);
    let frame = dec.read_next_frame().ok()??;
    // The first frame may cover only part of the logical screen: paste it at its offset onto
    // a transparent canvas of the full size.
    let mut canvas = vec![0u8; (w * h * 4) as usize];
    let (fx, fy, fw) = (frame.left as u32, frame.top as u32, frame.width as u32);
    for (i, px) in frame.buffer.chunks_exact(4).enumerate() {
        let (x, y) = (fx + i as u32 % fw, fy + i as u32 / fw);
        if x < w && y < h {
            let o = ((y * w + x) * 4) as usize;
            canvas[o..o + 4].copy_from_slice(px);
        }
    }
    let thumb = small(&canvas, w, h, false);
    // Add up the frame delays (hundredths of a second); only frame headers are read for the
    // remaining frames, which avoids decoding their pixels.
    let mut total = frame.delay as f64;
    while let Ok(Some(f)) = dec.next_frame_info() {
        total += f.delay as f64;
    }
    Some(((total > 0.0).then_some(total / 100.0), Some(thumb)))
}

/// Scales a `w` x `h` frame down to at most [`THUMB_W`] pixels wide, keeping its aspect ratio.
///
/// `px` holds 4 bytes per pixel; `bgra` says whether they are BGRA (video frames) rather than
/// RGBA, in which case red and blue are swapped for egui. Frames are never scaled up.
fn small(px: &[u8], w: u32, h: u32, bgra: bool) -> egui::ColorImage {
    let tw = THUMB_W.min(w).max(1);
    let th = ((h as u64 * tw as u64 / w.max(1) as u64) as u32).max(1);
    let mut out =
        crate::media::convert::resize(px, w as usize, h as usize, tw as usize, th as usize);
    if bgra {
        crate::media::convert::swap_red_blue(&mut out);
    }
    egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], &out)
}

/// Deletes a file by moving it to the Recycle Bin, so that it can be restored.
///
/// A path that does not exist counts as success. Uses `SHFileOperationW` without any
/// confirmation, progress or error dialogs; failure (including a cancelled operation) is
/// returned as an error with the shell's result code.
fn move_to_recycle_bin(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    // `pFrom` is a list of paths ended by an empty entry, so it needs two NULs at the end.
    let mut from: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]);
    // FOF_ALLOWUNDO is what sends the file to the Recycle Bin instead of deleting it.
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    // SAFETY: `op` is fully initialised and `from` outlives the call.
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        anyhow::bail!("{} ({r})", tr("Could not delete the file."));
    }
    Ok(())
}
