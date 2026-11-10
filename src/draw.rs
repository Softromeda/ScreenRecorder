// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! CPU drawing helpers built on `tiny-skia` (shapes) and `ab_glyph` (text).
//!
//! The recorder uses them to composite the mouse cursor, click effects and keystroke labels
//! onto captured frames; the editor uses them to render annotations and title pages into
//! overlay images that the render engine blends onto the video. All coordinates and sizes are
//! in pixels of the target [`Pixmap`], colours are straight (non-premultiplied) RGBA
//! `[r, g, b, a]`, and drawing is anti-aliased and clipped to the pixmap.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PremultipliedColorU8, Rect,
    Stroke, Transform,
};

/// Returns the font bundled with egui (Ubuntu Light), used when no other font is chosen.
///
/// # Panics
/// Panics if the bundled font data is invalid, which cannot happen with a correct build.
pub fn default_font() -> FontArc {
    FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font")
}

/// Parses a user-supplied font file, falling back to [`default_font`] when `bytes` is `None`
/// or not a valid TrueType/OpenType font.
pub fn load_font(bytes: Option<Vec<u8>>) -> FontArc {
    bytes
        .and_then(|b| FontArc::try_from_vec(b).ok())
        .unwrap_or_else(default_font)
}

/// Converts an RGBA byte array into a `tiny-skia` colour.
pub fn color(c: [u8; 4]) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

/// Returns an anti-aliased solid paint of colour `c`.
pub fn paint(c: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color(c));
    p.anti_alias = true;
    p
}

/// Measures a single line of text at pixel size `size`; returns `(width, height)` in pixels.
///
/// The width includes kerning; the height is ascent plus descent (no line gap), matching what
/// [`draw_text`] covers below its `y`.
pub fn measure_text(font: &FontArc, size: f32, text: &str) -> (f32, f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            width += scaled.kern(p, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }
    (width, scaled.ascent() - scaled.descent())
}

/// Draws a single line of text with the top-left corner of its line box at (`x`, `y`).
///
/// `size` is the font size in pixels and `rgba` the text colour; its alpha scales the glyph
/// coverage. Glyphs are rasterised by `ab_glyph` and blended pixel by pixel, because
/// `tiny-skia` has no text support. Parts outside the pixmap are clipped.
pub fn draw_text(
    pixmap: &mut Pixmap,
    font: &FontArc,
    size: f32,
    x: f32,
    y: f32,
    text: &str,
    rgba: [u8; 4],
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut caret = x;
    let baseline = y + scaled.ascent();
    let mut prev = None;
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    for ch in text.chars() {
        // Advance the caret (with kerning against the previous glyph) and position the glyph.
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, id);
        }
        let glyph =
            id.with_scale_and_position(PxScale::from(size), ab_glyph::point(caret, baseline));
        caret += scaled.h_advance(id);
        prev = Some(id);
        // Glyphs without an outline (such as spaces) only advance the caret.
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        // Blend each covered pixel; coordinates from `draw` are relative to the glyph bounds.
        let bounds = outlined.px_bounds();
        let pixels = pixmap.pixels_mut();
        outlined.draw(|gx, gy, coverage| {
            let px = bounds.min.x as i32 + gx as i32;
            let py = bounds.min.y as i32 + gy as i32;
            if px < 0 || py < 0 || px >= pw || py >= ph {
                return;
            }
            let a = coverage.clamp(0.0, 1.0) * rgba[3] as f32 / 255.0;
            if a <= 0.0 {
                return;
            }
            let idx = (py * pw + px) as usize;
            blend_pixel(&mut pixels[idx], rgba, a);
        });
    }
}

/// Blends a straight-alpha colour with opacity `a` (0.0–1.0) over a premultiplied pixel
/// (source-over).
///
/// The colour channels are clamped to the resulting alpha so that the pixel stays a valid
/// premultiplied value.
fn blend_pixel(dst: &mut PremultipliedColorU8, rgba: [u8; 4], a: f32) {
    let inv = 1.0 - a;
    let r = rgba[0] as f32 * a + dst.red() as f32 * inv;
    let g = rgba[1] as f32 * a + dst.green() as f32 * inv;
    let b = rgba[2] as f32 * a + dst.blue() as f32 * inv;
    let al = 255.0 * a + dst.alpha() as f32 * inv;
    let al_u = al.round().clamp(0.0, 255.0) as u8;
    let clamp = |v: f32| (v.round().clamp(0.0, al_u as f32)) as u8;
    if let Some(c) = PremultipliedColorU8::from_rgba(clamp(r), clamp(g), clamp(b), al_u) {
        *dst = c;
    }
}

/// Fills a circle of radius `r` centred on (`cx`, `cy`); radii below 0.5 are drawn at 0.5.
pub fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, rgba: [u8; 4]) {
    if let Some(path) = PathBuilder::from_circle(cx, cy, r.max(0.5)) {
        pixmap.fill_path(
            &path,
            &paint(rgba),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Strokes the outline of a circle of radius `r` centred on (`cx`, `cy`) with a line `width`
/// pixels wide.
pub fn stroke_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, width: f32, rgba: [u8; 4]) {
    if let Some(path) = PathBuilder::from_circle(cx, cy, r.max(0.5)) {
        let stroke = Stroke {
            width,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint(rgba), &stroke, Transform::identity(), None);
    }
}

/// Builds the path of a rectangle with rounded corners of radius `r`.
///
/// The radius is limited to half the width and height. Corners are approximated with
/// quadratic curves. Returns `None` for a degenerate rectangle.
pub fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    // Clockwise from the end of the top-left corner.
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Fills a rectangle with rounded corners of radius `r` (see [`rounded_rect_path`]).
pub fn fill_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    rgba: [u8; 4],
) {
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.fill_path(
            &path,
            &paint(rgba),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Strokes a rectangle outline with sharp (mitred) corners, the line centred on the edges.
///
/// Width and height are at least 1 pixel so that thin rectangles still draw.
pub fn stroke_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, width: f32, rgba: [u8; 4]) {
    if let Some(rect) = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)) {
        let path = PathBuilder::from_rect(rect);
        let stroke = Stroke {
            width,
            line_join: LineJoin::Miter,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint(rgba), &stroke, Transform::identity(), None);
    }
}

/// Fills a rectangle; width and height are at least 1 pixel.
pub fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgba: [u8; 4]) {
    if let Some(rect) = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)) {
        pixmap.fill_rect(rect, &paint(rgba), Transform::identity(), None);
    }
}

/// Strokes the ellipse inscribed in the rectangle (`x`, `y`, `w`, `h`) with a line `width`
/// pixels wide.
pub fn stroke_ellipse(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    width: f32,
    rgba: [u8; 4],
) {
    if let Some(rect) = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)) {
        if let Some(path) = PathBuilder::from_oval(rect) {
            let stroke = Stroke {
                width,
                ..Stroke::default()
            };
            pixmap.stroke_path(&path, &paint(rgba), &stroke, Transform::identity(), None);
        }
    }
}

/// Draws a line from (`x1`, `y1`) to (`x2`, `y2`) with a filled triangular arrow head at the end.
///
/// The head length scales with the line `width` (at least 12 pixels) but never exceeds 60 % of
/// the arrow's length. Arrows shorter than one pixel are not drawn.
pub fn draw_arrow(
    pixmap: &mut Pixmap,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    width: f32,
    rgba: [u8; 4],
) {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    // Unit direction, head length and the point where the shaft meets the head.
    let (ux, uy) = (dx / len, dy / len);
    let head = (width * 4.0).max(12.0).min(len * 0.6);
    let base_x = x2 - ux * head;
    let base_y = y2 - uy * head;

    // Shaft: stops at the head's base so that its round cap does not poke through the tip.
    let mut pb = PathBuilder::new();
    pb.move_to(x1, y1);
    pb.line_to(base_x, base_y);
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint(rgba), &stroke, Transform::identity(), None);
    }

    // Head: a triangle from the tip to two points either side of the base, along the
    // perpendicular (px, py).
    let (px, py) = (-uy, ux);
    let half = head * 0.55;
    let mut pb = PathBuilder::new();
    pb.move_to(x2, y2);
    pb.line_to(base_x + px * half, base_y + py * half);
    pb.line_to(base_x - px * half, base_y - py * half);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(
            &path,
            &paint(rgba),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Draws a standard white arrow mouse pointer with a black outline and soft shadow.
///
/// (`x`, `y`) is the hot spot (the arrow's tip). `scale` multiplies the nominal 12 × 19 pixel
/// size, e.g. to match the monitor's DPI scaling.
pub fn draw_cursor(pixmap: &mut Pixmap, x: f32, y: f32, scale: f32) {
    // Outline of the classic arrow pointer in a 12 × 19 box, starting at the hot spot.
    const PTS: [(f32, f32); 8] = [
        (0.0, 0.0),
        (0.0, 16.0),
        (4.0, 12.5),
        (7.0, 19.0),
        (9.5, 18.0),
        (6.5, 11.5),
        (11.5, 11.5),
        (0.0, 0.0),
    ];
    let mut pb = PathBuilder::new();
    for (i, (px, py)) in PTS.iter().enumerate() {
        let (px, py) = (x + px * scale, y + py * scale);
        if i == 0 {
            pb.move_to(px, py);
        } else {
            pb.line_to(px, py);
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        // Translucent shadow offset down and right, then the white body, then the outline.
        let shadow = Transform::from_translate(1.5 * scale, 1.5 * scale);
        pixmap.fill_path(
            &path,
            &paint([0, 0, 0, 70]),
            FillRule::Winding,
            shadow,
            None,
        );
        pixmap.fill_path(
            &path,
            &paint([255, 255, 255, 255]),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let stroke = Stroke {
            width: 1.2 * scale,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &paint([0, 0, 0, 255]),
            &stroke,
            Transform::identity(),
            None,
        );
    }
}

/// Draws a rounded "pill" of colour `bg` with the text centred in it in colour `fg`, as used
/// for keystroke displays and captions.
///
/// (`cx`, `bottom`) is the bottom-centre anchor of the pill. Padding and corner radius scale
/// with the font `size`. The pill is kept at least 4 pixels from the left edge; long text may
/// still run past the right edge.
#[allow(clippy::too_many_arguments)]
pub fn draw_label(
    pixmap: &mut Pixmap,
    font: &FontArc,
    size: f32,
    cx: f32,
    bottom: f32,
    text: &str,
    fg: [u8; 4],
    bg: [u8; 4],
) {
    let (tw, th) = measure_text(font, size, text);
    let pad_x = size * 0.6;
    let pad_y = size * 0.35;
    let w = tw + pad_x * 2.0;
    let h = th + pad_y * 2.0;
    let x = (cx - w / 2.0).max(4.0);
    let y = bottom - h;
    fill_rounded_rect(pixmap, x, y, w, h, size * 0.4, bg);
    draw_text(pixmap, font, size, x + pad_x, y + pad_y, text, fg);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text drawing produces visible pixels and measuring returns a plausible size.
    #[test]
    fn text_renders_pixels() {
        let font = default_font();
        let mut pm = Pixmap::new(200, 60).unwrap();
        draw_text(&mut pm, &font, 32.0, 4.0, 4.0, "Hello", [255, 0, 0, 255]);
        assert!(pm.pixels().iter().any(|p| p.alpha() > 0));
        let (w, h) = measure_text(&font, 32.0, "Hello");
        assert!(w > 40.0 && h > 20.0);
    }

    /// The cursor and an arrow together cover a meaningful number of pixels.
    #[test]
    fn cursor_and_arrow_render() {
        let mut pm = Pixmap::new(100, 100).unwrap();
        draw_cursor(&mut pm, 10.0, 10.0, 1.5);
        draw_arrow(&mut pm, 0.0, 90.0, 90.0, 50.0, 4.0, [0, 255, 0, 255]);
        assert!(pm.pixels().iter().filter(|p| p.alpha() > 0).count() > 50);
    }
}

#[cfg(test)]
mod ui_glyph_tests {
    use ab_glyph::{Font, FontRef};

    /// Every non-ASCII symbol in the UI sources' string literals exists in a font that egui
    /// bundles, so icons never show up as empty boxes, whatever fonts Windows has.
    #[test]
    fn ui_symbols_are_in_bundled_fonts() {
        let fonts: Vec<FontRef<'static>> = [
            epaint_default_fonts::UBUNTU_LIGHT,
            epaint_default_fonts::HACK_REGULAR,
            epaint_default_fonts::NOTO_EMOJI_REGULAR,
            epaint_default_fonts::EMOJI_ICON,
            epaint_default_fonts::EGUI_ICONS,
        ]
        .into_iter()
        .map(|d| FontRef::try_from_slice(d).unwrap())
        .collect();
        let sources = [
            include_str!("app.rs"),
            include_str!("editor/mod.rs"),
            include_str!("tools.rs"),
            include_str!("selector.rs"),
            include_str!("overlay.rs"),
            include_str!("library.rs"),
        ];
        let mut missing = std::collections::BTreeSet::new();
        for src in sources {
            // Only look inside string literals: after splitting on double quotes, the
            // odd-numbered parts are literal contents. This is a heuristic, so comments in
            // these files should keep their double quotes paired.
            for (i, part) in src.split('"').enumerate() {
                if i % 2 == 0 {
                    continue;
                }
                for c in part.chars().filter(|c| !c.is_ascii()) {
                    if fonts.iter().all(|f| f.glyph_id(c).0 == 0) {
                        missing.insert(format!("{c} (U+{:04X})", c as u32));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "symbols without a bundled glyph: {missing:?}"
        );
    }
}
