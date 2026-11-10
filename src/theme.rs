// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Visual style of the egui user interface.
//!
//! [`apply`] installs the program's font sizes, spacing, corner radii and colours into both the
//! light and the dark egui theme; it is called once when the main window starts. The module also
//! provides the brand colours ([`ACCENT`], [`ACCENT_LIGHT`], [`RECORD_RED`]) and a few small
//! reusable widgets used throughout the windows: [`card`], [`caption`], [`tile`] (capture-mode
//! buttons), [`chip`] (on/off toggles), [`banner`] (status messages) and [`accent_rule`].

use egui::{
    Color32, CornerRadius, FontId, Margin, Rect, Response, RichText, Sense, Stroke, StrokeKind,
    Theme, Vec2,
};

/// Primary brand colour (indigo), matching the program icon; used for selections and
/// active widgets.
pub const ACCENT: Color32 = Color32::from_rgb(79, 70, 229);
/// Lighter variant of [`ACCENT`] for hover outlines, icons and links on dark backgrounds.
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(129, 120, 255);
/// Red used for the record button and other "recording in progress" indicators.
pub const RECORD_RED: Color32 = Color32::from_rgb(239, 35, 60);

/// Applies the program's look to both the light and the dark egui theme.
///
/// Shared settings (text sizes, spacing, corner radii, selection colour) go into every style;
/// panel and widget background colours are then set separately for each theme so that either
/// one can be chosen by the user or follow the system setting.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        // Text sizes in points for each egui text style.
        use egui::TextStyle::*;
        style
            .text_styles
            .insert(Heading, FontId::proportional(19.0));
        style.text_styles.insert(Body, FontId::proportional(14.0));
        style.text_styles.insert(Button, FontId::proportional(14.0));
        style.text_styles.insert(Monospace, FontId::monospace(13.0));
        // egui's "small" text (hints, paths) defaults to 9 pt, which is hard to read.
        style.text_styles.insert(Small, FontId::proportional(12.0));

        // Slightly roomier spacing and taller controls than egui's compact defaults.
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 8.0);
        s.button_padding = Vec2::new(12.0, 6.0);
        s.interact_size.y = 28.0;
        s.window_margin = Margin::same(12);
        s.menu_margin = Margin::same(8);
        s.slider_width = 150.0;
        s.combo_width = 120.0;

        let v = &mut style.visuals;
        // Hints are drawn with "weak" text; keep it readable rather than very faint.
        v.weak_text_alpha = 0.7;
        // Round every widget state the same way so hovering does not change the shape.
        let r = CornerRadius::same(8);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = r;
        }
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT_LIGHT;
    });

    // Dark theme: near-black panels with slightly lighter widget backgrounds.
    ctx.style_mut_of(Theme::Dark, |style| {
        let v = &mut style.visuals;
        v.panel_fill = Color32::from_rgb(23, 24, 29);
        v.window_fill = Color32::from_rgb(30, 32, 38);
        v.extreme_bg_color = Color32::from_rgb(15, 16, 20);
        v.faint_bg_color = Color32::from_rgb(34, 36, 43);
        v.widgets.inactive.bg_fill = Color32::from_rgb(42, 45, 54);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(42, 45, 54);
        v.widgets.hovered.bg_fill = Color32::from_rgb(54, 58, 70);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 58, 70);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_LIGHT);
        v.widgets.active.bg_fill = ACCENT;
        v.widgets.active.weak_bg_fill = ACCENT;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(48, 51, 60));
    });
    // Light theme: pale grey panels with white cards and inputs.
    ctx.style_mut_of(Theme::Light, |style| {
        let v = &mut style.visuals;
        v.panel_fill = Color32::from_rgb(244, 245, 249);
        v.window_fill = Color32::WHITE;
        v.extreme_bg_color = Color32::WHITE;
        v.faint_bg_color = Color32::WHITE;
        v.widgets.inactive.bg_fill = Color32::from_rgb(229, 231, 238);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(229, 231, 238);
        v.widgets.hovered.bg_fill = Color32::from_rgb(218, 221, 232);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(218, 221, 232);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.bg_fill = ACCENT;
        v.widgets.active.weak_bg_fill = ACCENT;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(222, 225, 233));
    });
}

/// Draws a rounded, outlined card that groups related controls and returns the closure's result.
///
/// The card stretches to the full available width so that stacked cards line up.
pub fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

/// Draws a small, bold, muted section caption above a group of controls.
pub fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().strong().weak());
}

/// Draws a large square button with an icon above its label and returns its response.
///
/// Used for the capture-mode buttons of the main window. `size` is the tile size in points.
/// The tile highlights on hover and press, is greyed out when the `ui` is disabled, and shows a
/// pointing-hand cursor.
pub fn tile(ui: &mut egui::Ui, icon: &str, label: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    // Skip painting when the tile is scrolled out of view.
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let v = ui.visuals();
        let hovered = resp.hovered() && enabled;
        // Background: tinted accent while pressed, hover colour while hovered.
        let fill = if resp.is_pointer_button_down_on() && enabled {
            ACCENT.gamma_multiply(0.35)
        } else if hovered {
            v.widgets.hovered.bg_fill
        } else {
            v.widgets.inactive.bg_fill
        };
        let stroke = if hovered {
            Stroke::new(1.5, ACCENT_LIGHT)
        } else {
            Stroke::NONE
        };
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(12),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        // Icon colour: the light accent reads well on dark backgrounds, the full accent on
        // light ones; a disabled tile uses the weak text colour.
        let text = if enabled {
            v.strong_text_color()
        } else {
            v.weak_text_color()
        };
        let icon_color = if enabled { ACCENT_LIGHT } else { text };
        let icon_color = if v.dark_mode { icon_color } else { ACCENT };
        p.text(
            rect.center() - Vec2::new(0.0, 12.0),
            egui::Align2::CENTER_CENTER,
            icon,
            FontId::proportional(26.0),
            if enabled { icon_color } else { text },
        );
        // Label below the icon, wrapped to the tile width; a multi-line label is shifted up by
        // half of its extra height so that it stays inside the tile.
        let galley = ui.painter().layout(
            label.to_owned(),
            FontId::proportional(13.0),
            text,
            rect.width() - 10.0,
        );
        let pos = egui::pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y + 14.0 - (galley.size().y - 16.0).max(0.0) / 2.0,
        );
        ui.painter().galley(pos, galley, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Draws a rounded on/off toggle chip with an icon and label, flipping `on` when clicked.
///
/// A chip that is on is filled with [`ACCENT`] and has white text. The returned response is
/// marked as changed when the value was toggled.
pub fn chip(ui: &mut egui::Ui, on: &mut bool, icon: &str, label: &str) -> Response {
    let text = RichText::new(format!("{icon}  {label}")).size(13.0);
    let text = if *on {
        text.color(Color32::WHITE)
    } else {
        text
    };
    let mut button = egui::Button::new(text)
        .corner_radius(CornerRadius::same(14))
        .min_size(Vec2::new(0.0, 28.0));
    if *on {
        button = button.fill(ACCENT);
    }
    let mut resp = ui.add(button);
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp
}

/// Draws a full-width message banner with a close button.
///
/// `error` selects a red banner with a warning sign; otherwise the banner is green with a tick.
/// Returns `true` in the frame in which the user clicks the close button.
pub fn banner(ui: &mut egui::Ui, error: bool, text: &str) -> bool {
    let color = if error {
        crate::util::error_color(ui)
    } else {
        crate::util::ok_color(ui)
    };
    let mut closed = false;
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Icon and wrapped message on the left, close button pinned to the right edge.
            ui.horizontal(|ui| {
                ui.label(RichText::new(if error { "⚠" } else { "✔" }).color(color));
                ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    closed = ui.small_button("✖").clicked();
                });
            });
        });
    closed
}

/// Paints a thin accent bar, 48 points long and 2 points high, intended as a header underline.
#[allow(dead_code)]
pub fn accent_rule(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 2.0), Sense::hover());
    let mid = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + 48.0, rect.max.y));
    ui.painter().rect_filled(mid, 1.0, ACCENT);
}
