// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Export of an edited video.
//!
//! [`export`] turns a [`Project`] and the user's [`ExportOptions`] into a
//! [`RenderJob`](super::render::RenderJob) for [`super::render`]: it lays out the kept source
//! sections and title pages, pre-renders annotations and title pages into bitmaps with
//! tiny-skia ([`render_annotation`], [`render_title`]), loads background music and, for the
//! vertical and square [`Shape`]s, computes the path of the pointer-following frame. The same
//! annotation renderer is used by the editor preview, so the preview matches the exported file.

use ab_glyph::FontArc;
use tiny_skia::Pixmap;

use super::project::{Annotation, AnnotationKind, Project, TitlePage};
use crate::draw;
use crate::settings::OutputFormat;

/// Shape (aspect ratio) of the exported video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The shape of the source video.
    Original,
    /// 9:16 portrait, for Shorts, TikTok and Reels.
    Vertical,
    /// 1:1.
    Square,
}

impl Shape {
    /// All shapes, in the order they are offered in the export dialog.
    pub const ALL: [Shape; 3] = [Shape::Original, Shape::Vertical, Shape::Square];

    /// Returns the translated name shown in the user interface.
    pub fn label(self) -> &'static str {
        use crate::i18n::tr;
        match self {
            Shape::Original => tr("Original"),
            Shape::Vertical => tr("Vertical 9:16 (Shorts, TikTok, Reels)"),
            Shape::Square => tr("Square 1:1"),
        }
    }

    /// Returns the aspect ratio as width / height, or `None` to keep the source's shape.
    pub fn aspect(self) -> Option<f64> {
        match self {
            Shape::Original => None,
            Shape::Vertical => Some(9.0 / 16.0),
            Shape::Square => Some(1.0),
        }
    }

    /// Returns the largest frame of this shape that fits in a `sw` × `sh` source.
    ///
    /// The result is `(width, height, width fraction, height fraction)`: the frame's size in
    /// source pixels and the same size as fractions (0..=1) of the source width and height.
    /// The frame spans the full source height when the source is wider than the shape, and the
    /// full width otherwise.
    pub fn frame(self, sw: u32, sh: u32) -> (f64, f64, f32, f32) {
        let (sw, sh) = (sw.max(2) as f64, sh.max(2) as f64);
        match self.aspect() {
            Some(a) if sw / sh > a => (sh * a, sh, (sh * a / sw) as f32, 1.0),
            Some(a) => (sw, sw / a, 1.0, (sw / a / sh) as f32),
            None => (sw, sh, 1.0, 1.0),
        }
    }
}

/// Settings chosen in the editor's export dialog.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Container and codec of the output file (MP4 or GIF).
    pub format: OutputFormat,
    /// Shape of the output frame.
    pub shape: Shape,
    /// MP4 encoding quality, 1..=100.
    pub quality: u32,
    /// Output size as a percentage of the (shaped) source frame; clamped to 10..=100.
    pub scale_percent: u32,
    /// GIF frame rate in frames per second; clamped to 1..=50.
    pub gif_fps: u32,
    /// Maximum GIF width in pixels (at least 64).
    pub gif_width: u32,
}

impl Default for ExportOptions {
    /// MP4 in the original shape at quality 80 and full size; GIFs at 12 fps, up to 800 px wide.
    fn default() -> Self {
        Self {
            format: OutputFormat::Mp4,
            shape: Shape::Original,
            quality: 80,
            scale_percent: 100,
            gif_fps: 12,
            gif_width: 800,
        }
    }
}

/// Rounds `v` down to an even number, with a minimum of 2, as H.264 frame sizes must be even.
pub fn even(v: u32) -> u32 {
    (v / 2 * 2).max(2)
}

/// Renders one annotation into a transparent `w` × `h` pixmap covering the whole frame.
///
/// Annotation geometry is stored as fractions of the frame, and font size and stroke width as
/// fractions of the frame height, so the same annotation renders correctly at any resolution.
/// Returns `None` for blur annotations (they are applied to the video pixels by the renderer,
/// not drawn as an image) and when the pixmap cannot be allocated (zero size).
pub fn render_annotation(a: &Annotation, w: u32, h: u32, font: &FontArc) -> Option<Pixmap> {
    if a.kind == AnnotationKind::Blur {
        return None;
    }
    let mut pm = Pixmap::new(w, h)?;
    // Convert the normalised geometry into pixels, with minimum sizes so that small frames
    // still show visible lines and readable text.
    let (fw, fh) = (w as f32, h as f32);
    let r = a.rect();
    let (rx, ry, rw, rh) = (r.x * fw, r.y * fh, r.w * fw, r.h * fh);
    let stroke = (a.stroke * fh).max(1.5);
    let size = (a.size * fh).max(8.0);
    match a.kind {
        AnnotationKind::Text => {
            // Each line gets a slightly offset translucent black copy underneath as a drop
            // shadow, so the text stays readable on any background.
            let mut y = a.y * fh;
            for line in a.text.lines() {
                draw::draw_text(
                    &mut pm,
                    font,
                    size,
                    a.x * fw + size * 0.05,
                    y + size * 0.05,
                    line,
                    [0, 0, 0, 160],
                );
                draw::draw_text(&mut pm, font, size, a.x * fw, y, line, a.color);
                y += size * 1.2;
            }
        }
        AnnotationKind::Callout => {
            // The box grows beyond the drawn rectangle if needed to fit the text plus padding.
            let lines: Vec<&str> = a.text.lines().collect();
            let tw = lines
                .iter()
                .map(|l| draw::measure_text(font, size, l).0)
                .fold(0.0, f32::max);
            let pad = size * 0.5;
            let bw = rw.max(tw + pad * 2.0);
            let bh = rh.max(lines.len().max(1) as f32 * size * 1.2 + pad * 2.0);
            // Soft shadow, then the filled box in the annotation colour.
            draw::fill_rounded_rect(
                &mut pm,
                rx + 3.0,
                ry + 3.0,
                bw,
                bh,
                size * 0.4,
                [0, 0, 0, 70],
            );
            draw::fill_rounded_rect(&mut pm, rx, ry, bw, bh, size * 0.4, a.color);
            // Pick dark or white text from the box's perceived luminance (Rec. 601 weights).
            let lum =
                0.299 * a.color[0] as f32 + 0.587 * a.color[1] as f32 + 0.114 * a.color[2] as f32;
            let fg = if lum > 150.0 {
                [20, 20, 20, 255]
            } else {
                [255, 255, 255, 255]
            };
            let mut y = ry + pad;
            for line in lines {
                draw::draw_text(&mut pm, font, size, rx + pad, y, line, fg);
                y += size * 1.2;
            }
        }
        AnnotationKind::Rectangle => draw::stroke_rect(&mut pm, rx, ry, rw, rh, stroke, a.color),
        AnnotationKind::Ellipse => draw::stroke_ellipse(&mut pm, rx, ry, rw, rh, stroke, a.color),
        AnnotationKind::Highlight => draw::fill_rect(&mut pm, rx, ry, rw, rh, a.color),
        // Arrows use the raw start/end points rather than the normalised box, so the
        // direction is preserved.
        AnnotationKind::Arrow => draw::draw_arrow(
            &mut pm,
            a.x * fw,
            a.y * fh,
            (a.x + a.w) * fw,
            (a.y + a.h) * fh,
            stroke,
            a.color,
        ),
        AnnotationKind::Step => {
            // A filled circle (with a shadow) centred in the box, at least large enough for
            // the number, with the step number centred in white.
            let radius = (rw.min(rh) / 2.0).max(size * 0.7);
            let (cx, cy) = (rx + rw / 2.0, ry + rh / 2.0);
            draw::fill_circle(&mut pm, cx + 2.0, cy + 2.0, radius, [0, 0, 0, 70]);
            draw::fill_circle(&mut pm, cx, cy, radius, a.color);
            let label = a.number.to_string();
            let ts = radius * 1.1;
            let (tw, th) = draw::measure_text(font, ts, &label);
            draw::draw_text(
                &mut pm,
                font,
                ts,
                cx - tw / 2.0,
                cy - th / 2.0,
                &label,
                [255, 255, 255, 255],
            );
        }
        AnnotationKind::Blur => {}
    }
    Some(pm)
}

/// Renders the fast-forward badge (two triangles and the speed, e.g. "4×") shown on sped-up
/// sections of a video `out_h` pixels high: its width, height and straight-alpha BGRA pixels.
///
/// Returns `None` if the pixmap cannot be allocated.
pub fn fast_badge(speed: f64, out_h: u32, font: &FontArc) -> Option<(usize, usize, Vec<u8>)> {
    // All dimensions derive from the badge height (6 % of the video height, at least 24 px),
    // so the badge looks the same at every output size. Layout: pad, icon, gap, text, pad.
    let h = (out_h as f32 * 0.06).max(24.0);
    let text = format!("{}×", speed.round() as u32);
    let size = h * 0.55;
    let (tw, _) = draw::measure_text(font, size, &text);
    let icon = h * 0.6;
    let pad = h * 0.3;
    let w = pad + icon + pad * 0.6 + tw + pad;
    let mut pm = Pixmap::new(w.ceil() as u32, h.ceil() as u32)?;
    draw::fill_rounded_rect(&mut pm, 0.0, 0.0, w, h, h * 0.3, [0, 0, 0, 170]);
    // Fast-forward icon: two triangles pointing right.
    let (top, bottom, mid) = ((h - icon) / 2.0, (h + icon) / 2.0, h / 2.0);
    for i in 0..2 {
        let x = pad + i as f32 * icon / 2.0;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(x, top);
        pb.line_to(x + icon / 2.0, mid);
        pb.line_to(x, bottom);
        pb.close();
        if let Some(path) = pb.finish() {
            pm.fill_path(
                &path,
                &draw::paint([255, 255, 255, 255]),
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }
    let tx = pad + icon + pad * 0.6;
    draw::draw_text(
        &mut pm,
        font,
        size,
        tx,
        (h - size) / 2.0,
        &text,
        [255, 255, 255, 255],
    );
    let (pw, ph) = (pm.width() as usize, pm.height() as usize);
    Some((pw, ph, pixmap_to_bgra(pm)))
}

/// Renders a title page as an opaque `w` × `h` pixmap.
///
/// The title (possibly several lines) and the optional subtitle are centred horizontally and
/// the whole text block is centred vertically. Text sizes are fractions of the frame height.
/// Returns `None` when the pixmap cannot be allocated.
pub fn render_title(t: &TitlePage, w: u32, h: u32, font: &FontArc) -> Option<Pixmap> {
    let mut pm = Pixmap::new(w, h)?;
    pm.fill(draw::color(t.background));
    let fh = h as f32;
    let title_size = fh * 0.09;
    let sub_size = fh * 0.045;
    // Height of the whole text block, used to centre it vertically.
    let lines: Vec<&str> = t.title.lines().collect();
    let total = lines.len() as f32 * title_size * 1.2
        + if t.subtitle.is_empty() {
            0.0
        } else {
            sub_size * 2.0
        };
    let mut y = (fh - total) / 2.0;
    for line in lines {
        let (tw, _) = draw::measure_text(font, title_size, line);
        draw::draw_text(
            &mut pm,
            font,
            title_size,
            (w as f32 - tw) / 2.0,
            y,
            line,
            t.foreground,
        );
        y += title_size * 1.2;
    }
    // The subtitle is smaller, set slightly apart and drawn at 80 % of the title's opacity.
    if !t.subtitle.is_empty() {
        y += sub_size * 0.6;
        let mut c = t.foreground;
        c[3] = (c[3] as f32 * 0.8) as u8;
        let (tw, _) = draw::measure_text(font, sub_size, &t.subtitle);
        draw::draw_text(
            &mut pm,
            font,
            sub_size,
            (w as f32 - tw) / 2.0,
            y,
            &t.subtitle,
            c,
        );
    }
    Some(pm)
}

/// Converts a tiny-skia pixmap (premultiplied RGBA) into straight-alpha BGRA bytes, the
/// layout the renderer composites onto video frames.
pub fn pixmap_to_bgra(pm: Pixmap) -> Vec<u8> {
    let mut px = pm.take_demultiplied();
    crate::media::convert::swap_red_blue(&mut px);
    px
}

/// Exports `project` to the file `out` with the given options.
///
/// Builds a [`RenderJob`](super::render::RenderJob) and runs it on the calling thread
/// (normally a background job thread) and reports progress through `job`. `font` is used for
/// annotations and title pages.
///
/// Fails if the source cannot be read or has no video, if the background music cannot be
/// decoded, if trimming and cuts leave nothing to export, or if rendering or encoding fails.
#[cfg(windows)]
pub fn export(
    project: &Project,
    opts: &ExportOptions,
    out: &std::path::Path,
    font: &FontArc,
    job: &crate::job::Job,
) -> anyhow::Result<()> {
    use super::project::Piece;
    use super::render::{self, Output, Overlay, Part, RenderJob};

    job.set_progress(0.0, crate::i18n::tr("Analysing video…"));
    let info = crate::media::mf::probe(&project.source)?;
    if !info.has_video {
        anyhow::bail!("The source has no video stream");
    }
    // Output size: the frame of the chosen shape, scaled and rounded to even dimensions.
    let (sw, sh) = (info.width, info.height);
    let scale = opts.scale_percent.clamp(10, 100) as f64 / 100.0;
    let (fw, fh, frac_w, frac_h) = opts.shape.frame(sw, sh);
    let (ow, oh) = (
        even((fw * scale).round() as u32),
        even((fh * scale).round() as u32),
    );
    // Vertical and square exports crop a narrower frame that follows the recorded pointer.
    let follow = (opts.shape != Shape::Original).then(|| {
        super::project::follow_path(
            &super::project::load_pointer(&project.source),
            info.duration,
            frac_w,
            frac_h,
        )
    });
    // Background music is decoded up front (GIFs have no audio, so it is skipped for them).
    // The decode uses its own throwaway job so its progress does not overwrite this one.
    let music = match &project.music {
        Some(m) if opts.format != OutputFormat::Gif => {
            job.set_progress(0.0, crate::i18n::tr("Loading the music…"));
            let samples = render::decode_audio(&m.path, &crate::job::Job::new("music"))
                .map_err(|e| anyhow::anyhow!("{}: {e:#}", m.path.display()))?;
            Some(std::cell::RefCell::new(
                crate::media::voice::MusicMixer::new(samples, m.volume, m.duck),
            ))
        }
        _ => None,
    };

    // Pre-render every annotation once at source resolution; blurs are passed as rectangles
    // because they depend on the frame underneath.
    job.set_progress(0.01, crate::i18n::tr("Rendering annotations…"));
    let mut overlays = vec![];
    for a in &project.annotations {
        if a.kind == AnnotationKind::Blur {
            overlays.push(Overlay::Blur {
                span: a.span,
                rect: a.rect(),
            });
        } else if let Some(pm) = render_annotation(a, sw, sh, font) {
            overlays.push(Overlay::Image {
                span: a.span,
                bgra: pixmap_to_bgra(pm),
            });
        }
    }
    // Translate the project's timeline into render parts: source clips (with their zoom) and
    // title pages rendered as still images at output size.
    let mut parts = vec![];
    for piece in project.pieces(info.duration) {
        match piece {
            Piece::Source { span, zoom, speed } => parts.push(Part::Clip {
                source: 0,
                start: span.start,
                end: span.end,
                zoom,
                speed: speed as f64,
            }),
            Piece::Title(i) => {
                let t = &project.titles[i];
                let pm = render_title(t, ow, oh, font)
                    .ok_or_else(|| anyhow::anyhow!("cannot render title"))?;
                parts.push(Part::Still {
                    bgra: pixmap_to_bgra(pm),
                    duration: t.duration,
                });
            }
        }
    }
    if parts.is_empty() {
        anyhow::bail!("Nothing left to export: the whole video is trimmed or cut.");
    }
    let output = match opts.format {
        OutputFormat::Gif => Output::Gif {
            fps: opts.gif_fps.clamp(1, 50) as f64,
            max_width: opts.gif_width.max(64),
        },
        _ => Output::Mp4 {
            quality: opts.quality,
        },
    };
    // One fast-forward badge per speed used by sped-up sections.
    let mut badges: Vec<(f64, usize, usize, Vec<u8>)> = vec![];
    for p in &parts {
        if let Part::Clip { speed, .. } = p {
            if *speed > 1.0 && !badges.iter().any(|b| b.0 == *speed) {
                if let Some((w, h, px)) = fast_badge(*speed, oh, font) {
                    badges.push((*speed, w, h, px));
                }
            }
        }
    }
    // The logo image, loaded here so a missing or broken file stops the export with a message.
    let logo = match &project.watermark {
        Some(wm) => {
            let img = image::open(&wm.path)
                .map_err(|e| anyhow::anyhow!("{}: {e}", wm.path.display()))?
                .to_rgba8();
            Some(render::Logo {
                image: img,
                corner: wm.corner,
                size: wm.size,
                opacity: wm.opacity,
            })
        }
        None => None,
    };
    render::render(
        &RenderJob {
            sources: vec![project.source.clone()],
            parts,
            width: ow,
            height: oh,
            // Guard against bogus or missing frame rates reported by the container.
            fps: info.fps.clamp(1.0, 60.0),
            overlays,
            speed: project.speed as f64,
            volume: project.volume,
            mute: project.mute,
            fade_in: project.fade_in as f64,
            fade_out: project.fade_out as f64,
            output,
            follow,
            music,
            badges,
            logo,
        },
        out,
        job,
    )
}

#[cfg(test)]
mod tests {
    use super::super::project::Span;
    use super::*;

    /// Every annotation kind except blur renders to a pixmap, and a default title page renders.
    #[test]
    fn annotations_render() {
        let font = draw::default_font();
        for kind in AnnotationKind::ALL {
            let a = Annotation::new(kind, Span::new(0.0, 1.0), (0.1, 0.1), (0.5, 0.5));
            let pm = render_annotation(&a, 320, 180, &font);
            assert_eq!(pm.is_none(), kind == AnnotationKind::Blur);
        }
        assert!(render_title(&TitlePage::new(0.0), 320, 180, &font).is_some());
    }
}
