// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Full-screen overlay for choosing what to record.
//!
//! Before recording a region, a window or a fixed-size area, the main window is turned into a
//! borderless window covering one monitor and [`Selector`] draws a dimmed screenshot of that
//! monitor into it. The user drags a rectangle, clicks a highlighted window, or places a
//! fixed-size frame; the result is reported as a [`SelectorResult`] holding a
//! [`CaptureRegion`] in monitor pixels, which the app then uses to start recording.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2};

use crate::geometry::{self, CaptureRegion, MonitorInfo, WindowInfo};
#[allow(unused_imports)]
use crate::i18n::{tf, tr};

/// How the user picks the area on the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    /// Drag a rectangle.
    Region,
    /// Click a window to record (and follow) it, or drag a rectangle as in `Region`.
    Window,
    /// Click to place a rectangle of a fixed size, given in physical pixels.
    Fixed {
        /// Width of the rectangle in physical pixels.
        width: u32,
        /// Height of the rectangle in physical pixels.
        height: u32,
    },
}

/// Outcome of one frame of the selection overlay.
pub enum SelectorResult {
    /// The user has not finished choosing yet.
    Pending,
    /// The user chose this area (clamped to the monitor).
    Selected(CaptureRegion),
    /// The user pressed Esc, right-clicked or closed the window.
    Cancelled,
}

/// State of the area-selection overlay for one monitor.
pub struct Selector {
    /// The selection mode the overlay was opened in.
    pub mode: SelectMode,
    /// The monitor being shown; selections are relative to it.
    monitor: MonitorInfo,
    /// Screenshot of the monitor taken when the overlay opened, drawn as the background.
    texture: TextureHandle,
    /// Candidate windows for [`SelectMode::Window`], topmost first (empty in other modes).
    windows: Vec<WindowInfo>,
    /// Where the current drag started, in egui points; `None` when not dragging.
    drag_start: Option<Pos2>,
    /// `None` while selecting, `Some(None)` when cancelled, `Some(Some(region))` when done.
    result: Option<Option<CaptureRegion>>,
    /// Number of frames drawn so far; used to grab focus during the first frames.
    frames: u32,
}

impl Selector {
    /// Captures the monitor under the mouse (or the primary monitor) and prepares the overlay.
    ///
    /// `mouse` is the cursor position in virtual-desktop pixels. In window mode the list of
    /// top-level windows is taken at the same time, excluding this program's own windows.
    /// Fails if no monitor is found or the screenshot cannot be taken.
    pub fn new(
        ctx: &egui::Context,
        mode: SelectMode,
        mouse: Option<(i32, i32)>,
    ) -> anyhow::Result<Self> {
        let monitor = mouse
            .and_then(|(x, y)| geometry::monitor_at(x, y))
            .or_else(geometry::primary_monitor)
            .ok_or_else(|| anyhow::anyhow!("No monitor found"))?;
        // Screenshot the monitor before the overlay covers it and upload it as a texture.
        let m = geometry::find_monitor(monitor.id)
            .ok_or_else(|| anyhow::anyhow!("Monitor unavailable"))?;
        let shot = m
            .capture_image()
            .map_err(|e| anyhow::anyhow!("Screen capture failed: {e}"))?;
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [shot.width() as usize, shot.height() as usize],
            shot.as_raw(),
        );
        let texture = ctx.load_texture("selector-screenshot", img, egui::TextureOptions::LINEAR);
        // Our own windows (the main window, the overlay itself) must not be offered as targets.
        let windows = if mode == SelectMode::Window {
            geometry::list_windows()
                .into_iter()
                .filter(|w| {
                    !w.title.contains(crate::util::APP_NAME) && !w.title.contains("Select area")
                })
                .collect()
        } else {
            vec![]
        };
        Ok(Self {
            mode,
            monitor,
            texture,
            windows,
            drag_start: None,
            result: None,
            frames: 0,
        })
    }

    /// Returns the monitor rectangle in logical points; the overlay window is placed here.
    pub fn logical_rect(&self) -> Rect {
        self.monitor.logical_rect()
    }

    /// Draws the overlay into the full-screen, borderless main window and reports the outcome.
    pub fn show(&mut self, ui: &mut egui::Ui) -> SelectorResult {
        self.ui(ui);
        match self.result {
            None => SelectorResult::Pending,
            Some(Some(r)) => SelectorResult::Selected(r),
            Some(None) => SelectorResult::Cancelled,
        }
    }

    /// Draws one frame of the overlay and handles input, storing the outcome in `self.result`.
    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Ask for keyboard focus during the first frames so that Esc works immediately.
        self.frames += 1;
        if self.frames < 3 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Background: the screenshot, dimmed so that the selected area stands out.
        let full = ui.max_rect();
        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        let painter = ui.painter_at(full);
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        painter.image(self.texture.id(), full, uv, Color32::WHITE);
        painter.rect_filled(full, 0.0, Color32::from_black_alpha(110));

        // Conversions between egui points in this window and monitor-local physical pixels;
        // the window covers the monitor exactly, so the scale is its pixels-per-point.
        let ppp = ctx.pixels_per_point();
        let upp = ppp;
        let to_units = |p: Pos2| ((p.x - full.left()) * upp, (p.y - full.top()) * upp);
        let to_points = |x: f32, y: f32| Pos2::new(full.left() + x / upp, full.top() + y / upp);

        // Cancel on Esc, right-click or a request to close the window.
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if esc || resp.secondary_clicked() || ctx.input(|i| i.viewport().close_requested()) {
            self.result = Some(None);
            return;
        }

        // The rectangle that would be selected if the user committed in this frame, in egui points.
        let mut candidate: Option<Rect> = None;
        // Window under the mouse (Window mode, not dragging).
        let mut hovered: Option<u32> = None;
        match self.mode {
            SelectMode::Fixed { width, height } => {
                // A fixed-size frame centred on the pointer.
                if let Some(p) = pointer {
                    let size = Vec2::new(width as f32 / upp, height as f32 / upp);
                    let mut r = Rect::from_center_size(p, size);
                    // Shift the frame back inside the monitor where it overhangs an edge.
                    r = r.translate(Vec2::new(
                        (full.left() - r.left()).max(0.0) + (full.right() - r.right()).min(0.0),
                        (full.top() - r.top()).max(0.0) + (full.bottom() - r.bottom()).min(0.0),
                    ));
                    candidate = Some(r);
                }
            }
            SelectMode::Region | SelectMode::Window => {
                if let (Some(start), Some(p)) = (self.drag_start, pointer) {
                    // Dragging: the rectangle between the drag origin and the pointer.
                    candidate = Some(Rect::from_two_pos(start, p));
                } else if self.mode == SelectMode::Window {
                    // Hovering in window mode: find the topmost window under the pointer
                    // (compared in virtual-desktop pixels) and highlight it.
                    if let Some(p) = pointer {
                        let (ux, uy) = to_units(p);
                        let gx = self.monitor.x + ux as i32;
                        let gy = self.monitor.y + uy as i32;
                        if let Some(w) = self.windows.iter().find(|w| {
                            gx >= w.x
                                && gy >= w.y
                                && gx < w.x + w.width as i32
                                && gy < w.y + w.height as i32
                        }) {
                            let a = to_points(
                                (w.x - self.monitor.x) as f32,
                                (w.y - self.monitor.y) as f32,
                            );
                            let b = to_points(
                                (w.x - self.monitor.x + w.width as i32) as f32,
                                (w.y - self.monitor.y + w.height as i32) as f32,
                            );
                            candidate = Some(Rect::from_two_pos(a, b).intersect(full));
                            hovered = Some(w.id);
                            // Label the highlighted window with its application and title.
                            if !w.title.is_empty() {
                                painter.text(
                                    candidate.unwrap().left_top() + Vec2::new(6.0, 6.0),
                                    Align2::LEFT_TOP,
                                    format!("{} — {}", w.app_name, w.title),
                                    FontId::proportional(13.0),
                                    Color32::WHITE,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Remember where a drag began; the press origin is more precise than the position at
        // which egui recognises the drag.
        if resp.drag_started() && !matches!(self.mode, SelectMode::Fixed { .. }) {
            self.drag_start = ctx
                .input(|i| i.pointer.press_origin())
                .or_else(|| resp.interact_pointer_pos());
        }

        // Draw the candidate area un-dimmed by repainting that part of the screenshot on top,
        // outline it, and show its size in pixels just above it.
        if let Some(r) = candidate {
            let uv = Rect::from_min_max(
                Pos2::new(
                    (r.left() - full.left()) / full.width(),
                    (r.top() - full.top()) / full.height(),
                ),
                Pos2::new(
                    (r.right() - full.left()) / full.width(),
                    (r.bottom() - full.top()) / full.height(),
                ),
            );
            painter.image(self.texture.id(), r, uv, Color32::WHITE);
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(2.0, Color32::from_rgb(255, 214, 0)),
                StrokeKind::Outside,
            );
            let (w, h) = (r.width() * upp, r.height() * upp);
            let label = format!("{} × {}", w.round() as i32, h.round() as i32);
            let pos = Pos2::new(r.left(), (r.top() - 22.0).max(full.top() + 2.0));
            // Approximate the label width from its length rather than laying it out.
            let galley_rect =
                Rect::from_min_size(pos, Vec2::new(label.len() as f32 * 8.0 + 10.0, 20.0));
            painter.rect_filled(galley_rect, 3.0, Color32::from_black_alpha(180));
            painter.text(
                pos + Vec2::new(5.0, 3.0),
                Align2::LEFT_TOP,
                label,
                FontId::monospace(13.0),
                Color32::WHITE,
            );
        }

        // Full-screen crosshair through the pointer before a region drag starts.
        if let Some(p) = pointer {
            if self.drag_start.is_none() && self.mode == SelectMode::Region {
                let s = Stroke::new(1.0, Color32::from_white_alpha(140));
                painter.line_segment(
                    [Pos2::new(full.left(), p.y), Pos2::new(full.right(), p.y)],
                    s,
                );
                painter.line_segment(
                    [Pos2::new(p.x, full.top()), Pos2::new(p.x, full.bottom())],
                    s,
                );
            }
        }
        // Instruction pill at the top centre of the monitor.
        let hint = match self.mode {
            SelectMode::Region => {
                tr("Drag to select the area to record  •  Esc / right-click to cancel")
            }
            SelectMode::Window => {
                tr("Click a window (or drag a region) to record it  •  Esc to cancel")
            }
            SelectMode::Fixed { .. } => {
                tr("Click to place the fixed-size region  •  Esc to cancel")
            }
        };
        let hint_pos = Pos2::new(full.center().x, full.top() + 30.0);
        painter.rect_filled(
            Rect::from_center_size(hint_pos, Vec2::new(hint.len() as f32 * 7.0 + 24.0, 30.0)),
            6.0,
            Color32::from_black_alpha(190),
        );
        painter.text(
            hint_pos,
            Align2::CENTER_CENTER,
            hint,
            FontId::proportional(14.0),
            Color32::WHITE,
        );

        // Commit: a click places a fixed frame, releasing a drag ends a region, and window mode
        // accepts either. The candidate is converted to monitor pixels and clamped; a region
        // too small to record is discarded and the user can try again.
        let commit = match self.mode {
            SelectMode::Fixed { .. } => resp.clicked(),
            SelectMode::Region => resp.drag_stopped(),
            SelectMode::Window => resp.clicked() || resp.drag_stopped(),
        };
        if commit {
            if let Some(r) = candidate {
                let (x0, y0) = to_units(r.min);
                let (x1, y1) = to_units(r.max);
                let region = geometry::clamp_region(
                    &self.monitor,
                    x0.round() as i64,
                    y0.round() as i64,
                    (x1 - x0).round() as i64,
                    (y1 - y0).round() as i64,
                );
                if let Some(mut region) = region {
                    // A clicked window is recorded itself and followed when it moves; a
                    // dragged rectangle is a plain screen region.
                    if self.mode == SelectMode::Window && resp.clicked() {
                        region.window = hovered.map(|id| id as isize);
                    }
                    self.result = Some(Some(region));
                    return;
                }
            }
            self.drag_start = None;
        }
        // Keep repainting so that the highlight follows the pointer smoothly.
        ctx.request_repaint();
    }
}
