// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Data model of an editor project.
//!
//! A [`Project`] records every edit made to one source video: trim, cuts, [`Zoom`]s,
//! [`Annotation`]s, [`TitlePage`]s, speed, volume, fades and background [`Music`]. It is
//! auto-saved as JSON next to the video (`<video>.srproj.json`), so edits stay re-editable, and
//! is turned into an ordered list of output [`Piece`]s for preview and export. The module also
//! reads the click and pointer logs written during recording ([`Click`]) and derives automatic
//! zooms ([`auto_zooms`]) and the pointer-following crop path for vertical and square exports
//! ([`follow_path`]). Times are in seconds of the source video unless stated otherwise;
//! positions are fractions (0..=1) of the frame width and height.

use std::path::{Path, PathBuf};

use crate::i18n::tr;
use serde::{Deserialize, Serialize};

use crate::settings::Rgba;

/// A time interval `[start, end)` in seconds; `start <= end` when built with [`Span::new`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Inclusive start time in seconds.
    pub start: f64,
    /// Exclusive end time in seconds.
    pub end: f64,
}

impl Span {
    /// Creates a span between two times given in either order.
    pub fn new(a: f64, b: f64) -> Self {
        Self {
            start: a.min(b),
            end: a.max(b),
        }
    }
    /// Returns the length in seconds (never negative).
    pub fn len(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
    /// Returns whether `t` lies in the half-open interval `[start, end)`.
    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }
}

/// A rectangle in normalised frame coordinates: every value is a fraction (0..=1) of the frame
/// width (`x`, `w`) or height (`y`, `h`), with the origin at the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl NRect {
    /// Creates the rectangle spanned by two opposite corners given in any order, clamped to the
    /// frame.
    pub fn from_points(a: (f32, f32), b: (f32, f32)) -> Self {
        let x0 = a.0.min(b.0).clamp(0.0, 1.0);
        let y0 = a.1.min(b.1).clamp(0.0, 1.0);
        let x1 = a.0.max(b.0).clamp(0.0, 1.0);
        let y1 = a.1.max(b.1).clamp(0.0, 1.0);
        Self {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }

    /// Converts the rectangle to pixels `(x, y, width, height)` in a `w` × `h` frame.
    ///
    /// All four values are rounded down to even numbers (as required for NV12/H.264 chroma
    /// subsampling), the size is at least 2 × 2 and the rectangle stays inside the frame.
    /// The frame itself must be at least 2 × 2 pixels.
    pub fn to_pixels_even(self, w: u32, h: u32) -> (u32, u32, u32, u32) {
        // Round down to a multiple of two.
        let even = |v: f32| ((v / 2.0).floor() as u32) * 2;
        let px = even(self.x * w as f32).min(w.saturating_sub(2));
        let py = even(self.y * h as f32).min(h.saturating_sub(2));
        let pw = even(self.w * w as f32).clamp(2, w - px);
        let ph = even(self.h * h as f32).clamp(2, h - py);
        (px, py, pw, ph)
    }
}

/// The kind of an [`Annotation`], which decides how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationKind {
    /// Free text with a drop shadow, starting at the box's top-left corner.
    Text,
    /// Text in a filled rounded box (a speech-bubble style label).
    Callout,
    /// Outlined rectangle.
    Rectangle,
    /// Outlined ellipse.
    Ellipse,
    /// Arrow from a start point to an end point.
    Arrow,
    /// Translucent filled rectangle, like a highlighter pen.
    Highlight,
    /// Blurs the video inside the box, to hide sensitive information.
    Blur,
    /// Numbered circle for marking the steps of a procedure.
    Step,
}

impl AnnotationKind {
    /// All kinds, in the order they appear in the editor's toolbar.
    pub const ALL: [AnnotationKind; 8] = [
        AnnotationKind::Text,
        AnnotationKind::Callout,
        AnnotationKind::Arrow,
        AnnotationKind::Rectangle,
        AnnotationKind::Ellipse,
        AnnotationKind::Highlight,
        AnnotationKind::Blur,
        AnnotationKind::Step,
    ];

    /// Returns the translated name shown in the user interface.
    pub fn label(self) -> &'static str {
        match self {
            AnnotationKind::Text => tr("Text"),
            AnnotationKind::Callout => tr("Callout"),
            AnnotationKind::Rectangle => tr("Rectangle"),
            AnnotationKind::Ellipse => tr("Ellipse"),
            AnnotationKind::Arrow => tr("Arrow"),
            AnnotationKind::Highlight => tr("Highlight"),
            AnnotationKind::Blur => tr("Blur"),
            AnnotationKind::Step => tr("Step number"),
        }
    }

    /// Returns whether annotations of this kind carry editable text.
    pub fn has_text(self) -> bool {
        matches!(self, AnnotationKind::Text | AnnotationKind::Callout)
    }
}

/// A graphic or text drawn over the video for a span of source time.
///
/// The geometry fields `x`, `y`, `w`, `h` are in normalised frame coordinates (see [`NRect`]).
/// For arrows they describe a start point `(x, y)` and an end point `(x + w, y + h)`, so `w` and
/// `h` may be negative; for all other kinds they describe a box with non-negative size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    /// What is drawn.
    pub kind: AnnotationKind,
    /// Source time range during which the annotation is visible.
    pub span: Span,
    /// Left edge of the box, or the arrow's start x.
    pub x: f32,
    /// Top edge of the box, or the arrow's start y.
    pub y: f32,
    /// Box width, or the arrow's horizontal extent (may be negative).
    pub w: f32,
    /// Box height, or the arrow's vertical extent (may be negative).
    pub h: f32,
    /// Colour (RGBA): text colour for text, fill for callouts, highlights and steps, line colour
    /// otherwise.
    pub color: Rgba,
    /// Text of text and callout annotations (may contain several lines).
    pub text: String,
    /// Font size as a fraction of the frame height.
    pub size: f32,
    /// Line width as a fraction of the frame height.
    pub stroke: f32,
    /// Number shown by a step annotation.
    pub number: u32,
}

impl Annotation {
    /// Creates an annotation of `kind` dragged from point `a` to point `b` (normalised
    /// coordinates), visible during `span`, with the default colour, text and sizes for its kind.
    pub fn new(kind: AnnotationKind, span: Span, a: (f32, f32), b: (f32, f32)) -> Self {
        let (x, y, w, h) = if kind == AnnotationKind::Arrow {
            (a.0, a.1, b.0 - a.0, b.1 - a.1)
        } else {
            let r = NRect::from_points(a, b);
            (r.x, r.y, r.w, r.h)
        };
        // Defaults: translucent yellow highlighter, white text, dark callout box, red otherwise.
        let color = match kind {
            AnnotationKind::Highlight => [255, 235, 59, 90],
            AnnotationKind::Text => [255, 255, 255, 255],
            AnnotationKind::Callout => [33, 33, 33, 255],
            _ => [229, 57, 53, 255],
        };
        Self {
            kind,
            span,
            x,
            y,
            w,
            h,
            color,
            text: if kind.has_text() {
                "Your text".into()
            } else {
                String::new()
            },
            size: 0.05,
            stroke: 0.006,
            number: 1,
        }
    }

    /// Returns the annotation's bounding box as a normalised rectangle with positive size
    /// (for arrows, the box spanned by the start and end points).
    pub fn rect(&self) -> NRect {
        NRect::from_points((self.x, self.y), (self.x + self.w, self.y + self.h))
    }
}

/// A zoom-and-pan effect: the view moves into `rect` during `span` and back out again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zoom {
    /// Source time range of the effect, including the ease-in and ease-out.
    pub span: Span,
    /// Area of the frame shown when fully zoomed in.
    pub rect: NRect,
}

impl Zoom {
    /// Returns the visible area at source time `t`.
    ///
    /// The view eases in from the whole frame to `rect` at the start of the span and back out at
    /// the end. Each transition takes a third of the span, but at most 0.45 s.
    pub fn rect_at(&self, t: f64) -> NRect {
        let ramp = (self.span.len() / 3.0).min(0.45);
        if ramp <= 0.0 {
            return self.rect;
        }
        // Progress 0..=1: rising over the first ramp, falling over the last one.
        let k = ((t - self.span.start) / ramp)
            .min((self.span.end - t) / ramp)
            .clamp(0.0, 1.0);
        // Smoothstep for a gentle start and stop, then interpolate from the full frame.
        let k = (k * k * (3.0 - 2.0 * k)) as f32;
        let lerp = |a: f32, b: f32| a + (b - a) * k;
        NRect {
            x: lerp(0.0, self.rect.x),
            y: lerp(0.0, self.rect.y),
            w: lerp(1.0, self.rect.w),
            h: lerp(1.0, self.rect.h),
        }
    }
}

/// A pointer sample recorded with a video: a mouse click, or a mouse position.
///
/// Values outside 0..=1 mean the pointer was outside the recorded area.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Click {
    /// Time in seconds from the start of the video.
    pub t: f64,
    /// Horizontal position as a fraction of the video width.
    pub x: f32,
    /// Vertical position as a fraction of the video height.
    pub y: f32,
}

/// Returns the path of the click log saved next to a recording (`<video>.clicks.json`).
pub fn clicks_file(video: &Path) -> PathBuf {
    let mut s = video.as_os_str().to_owned();
    s.push(".clicks.json");
    PathBuf::from(s)
}

/// Loads the clicks recorded with `video`, or `None` if the file is missing or unreadable.
pub fn load_clicks(video: &Path) -> Option<Vec<Click>> {
    let text = std::fs::read_to_string(clicks_file(video)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Path of the markers saved next to a recording (`<video>.markers.json`): a JSON list of
/// times in seconds.
pub fn markers_file(video: &Path) -> PathBuf {
    let mut s = video.as_os_str().to_owned();
    s.push(".markers.json");
    PathBuf::from(s)
}

/// Loads the markers saved with a recording (sorted times in seconds); empty if there are none.
pub fn load_markers(video: &Path) -> Vec<f64> {
    let mut m: Vec<f64> = std::fs::read_to_string(markers_file(video))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    m.sort_by(|a, b| a.total_cmp(b));
    m
}

/// Returns the path of the pointer log saved next to a recording (`<video>.mouse.json`).
///
/// It holds mouse positions sampled a few times per second, stored as [`Click`]s.
pub fn mouse_file(video: &Path) -> PathBuf {
    let mut s = video.as_os_str().to_owned();
    s.push(".mouse.json");
    PathBuf::from(s)
}

/// Loads where the action is over time for `video`.
///
/// Returns the recorded mouse positions, falling back to the clicks when there is no (or an
/// empty) pointer log, and an empty list when neither exists.
pub fn load_pointer(video: &Path) -> Vec<Click> {
    std::fs::read_to_string(mouse_file(video))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Click>>(&t).ok())
        .filter(|v| !v.is_empty())
        .or_else(|| load_clicks(video))
        .unwrap_or_default()
}

/// Computes the path of a narrower crop frame (vertical or square export) that follows the
/// pointer like a camera operator.
///
/// `w` and `h` are the crop's size as fractions of the source frame. The result holds the
/// crop's centre, as fractions of the source, sampled every 1/30 s from 0 to `duration`
/// inclusive; look values up with [`follow_at`]. The crop only moves when the pointer leaves its
/// middle 60 %, moves smoothly, and always stays inside the source frame.
pub fn follow_path(pointer: &[Click], duration: f64, w: f32, h: f32) -> Vec<(f32, f32)> {
    /// Samples per second of the returned path (must match [`follow_at`]).
    const RATE: f64 = 30.0;
    let (w, h) = (w.clamp(0.05, 1.0), h.clamp(0.05, 1.0));
    // Keep a centre far enough from the edges for the crop to fit inside the source.
    let clamp = |c: (f32, f32)| {
        (
            c.0.clamp(w / 2.0, 1.0 - w / 2.0),
            c.1.clamp(h / 2.0, 1.0 - h / 2.0),
        )
    };
    // Ignore samples outside the recorded area and process them in time order.
    let mut pts: Vec<Click> = pointer
        .iter()
        .copied()
        .filter(|p| (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y))
        .collect();
    pts.sort_by(|a, b| a.t.total_cmp(&b.t));
    let steps = (duration.max(0.0) * RATE).ceil() as usize + 1;
    let mut cam = clamp(pts.first().map_or((0.5, 0.5), |p| (p.x, p.y)));
    let mut next = 0;
    let mut target = cam;
    // Per-step smoothing factor of an exponential follow with a 0.35 s time constant, so the
    // crop catches up in roughly a third of a second.
    let k = 1.0 - (-1.0 / (RATE * 0.35)).exp() as f32;
    (0..steps)
        .map(|i| {
            let t = i as f64 / RATE;
            // The target is the latest pointer sample at or before this time.
            while next < pts.len() && pts[next].t <= t {
                target = (pts[next].x, pts[next].y);
                next += 1;
            }
            // Dead zone: the crop only moves when the pointer leaves its middle 60 %, and then
            // just far enough to bring the pointer back to the zone's edge.
            let dead = (w * 0.3, h * 0.3);
            let want = |c: f32, p: f32, d: f32| {
                if p > c + d {
                    p - d
                } else if p < c - d {
                    p + d
                } else {
                    c
                }
            };
            let goal = clamp((want(cam.0, target.0, dead.0), want(cam.1, target.1, dead.1)));
            cam = (cam.0 + (goal.0 - cam.0) * k, cam.1 + (goal.1 - cam.1) * k);
            cam
        })
        .collect()
}

/// Returns the crop centre at source time `t` from a path made by [`follow_path`].
///
/// Times outside the path are clamped to its ends; an empty path yields the frame centre.
pub fn follow_at(path: &[(f32, f32)], t: f64) -> (f32, f32) {
    if path.is_empty() {
        return (0.5, 0.5);
    }
    let i = ((t.max(0.0) * 30.0).round() as usize).min(path.len() - 1);
    path[i]
}

/// Creates zooms around recorded clicks, used when a new recording opens in the editor.
///
/// Clicks close together in time and place share one zoom. Each zoom frames its clicks at 2×
/// (or wider when they are spread out, up to 80 % of the frame), keeps the frame's aspect ratio,
/// starts a little before the first click and ends a while after the last one. Zooms never
/// overlap, end by `duration` and last at least a second. Clicks outside the video or at
/// negative times are ignored.
pub fn auto_zooms(clicks: &[Click], duration: f64) -> Vec<Zoom> {
    const BEFORE: f64 = 0.7; // start zooming this long (s) before the first click
    const AFTER: f64 = 1.8; // stay zoomed this long (s) after the last click
    const GAP: f64 = 1.5; // a click this soon (s) after the zoom would end joins the group
    const LEVEL: f32 = 0.5; // visible part of the frame (0.5 = 2×)
    const MAX_SPREAD: f32 = 0.55; // a group never spans more than this fraction of the frame
    let mut clicks: Vec<Click> = clicks
        .iter()
        .copied()
        .filter(|c| (0.0..=1.0).contains(&c.x) && (0.0..=1.0).contains(&c.y) && c.t >= 0.0)
        .collect();
    clicks.sort_by(|a, b| a.t.total_cmp(&b.t));

    // Group the clicks: a click joins the current group when it follows the group's last click
    // closely enough and the group's bounding box, including it, stays small enough.
    let mut groups: Vec<Vec<Click>> = vec![];
    for c in clicks {
        if let Some(g) = groups.last_mut() {
            let last = g.last().unwrap();
            let (mut x0, mut x1, mut y0, mut y1) = (c.x, c.x, c.y, c.y);
            for o in g.iter() {
                (x0, x1, y0, y1) = (x0.min(o.x), x1.max(o.x), y0.min(o.y), y1.max(o.y));
            }
            let close_in_time = c.t - last.t <= AFTER + GAP;
            let close_in_space = x1 - x0 <= MAX_SPREAD && y1 - y0 <= MAX_SPREAD;
            if close_in_time && close_in_space {
                g.push(c);
                continue;
            }
        }
        groups.push(vec![c]);
    }

    // Turn each group into a zoom around the bounding box of its clicks.
    let mut zooms: Vec<Zoom> = vec![];
    for g in groups {
        let (mut x0, mut x1, mut y0, mut y1) = (1.0f32, 0.0f32, 1.0f32, 0.0f32);
        for c in &g {
            (x0, x1, y0, y1) = (x0.min(c.x), x1.max(c.x), y0.min(c.y), y1.max(c.y));
        }
        // Same fraction for width and height keeps the frame's shape; add a margin around the
        // clicks and cap the size so there is always some zoom.
        let size = LEVEL.max(x1 - x0 + 0.2).max(y1 - y0 + 0.2).min(0.8);
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let rect = NRect {
            x: (cx - size / 2.0).clamp(0.0, 1.0 - size),
            y: (cy - size / 2.0).clamp(0.0, 1.0 - size),
            w: size,
            h: size,
        };
        let start = (g[0].t - BEFORE).max(0.0);
        let end = (g[g.len() - 1].t + AFTER).min(duration);
        // Overlapping the previous zoom: end that one early (but keep at least a second of it)
        // and start this one after it.
        let (start, prev_end) = match zooms.last() {
            Some(prev) if start < prev.span.end => {
                let prev_end = start.max(prev.span.start + 1.0).min(prev.span.end);
                (start.max(prev_end), Some(prev_end))
            }
            _ => (start, None),
        };
        // Drop zooms shorter than a second; only then is the previous zoom shortened.
        if end - start >= 1.0 {
            if let (Some(prev), Some(e)) = (zooms.last_mut(), prev_end) {
                prev.span.end = e;
            }
            zooms.push(Zoom {
                span: Span::new(start, end),
                rect,
            });
        }
    }
    zooms
}

/// A full-frame title card inserted into the output timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitlePage {
    /// Source time before which the page is inserted, in seconds.
    pub at: f64,
    /// How long the page is shown, in seconds of output (before the speed change).
    pub duration: f64,
    /// Main text, centred; may contain several lines.
    pub title: String,
    /// Smaller line below the title; empty for none.
    pub subtitle: String,
    /// Background colour.
    pub background: Rgba,
    /// Text colour (the subtitle is drawn slightly more transparent).
    pub foreground: Rgba,
}

impl TitlePage {
    /// Creates a three-second white-on-navy title page inserted at source time `at`.
    pub fn new(at: f64) -> Self {
        Self {
            at,
            duration: 3.0,
            title: "Title".into(),
            subtitle: String::new(),
            background: [25, 40, 72, 255],
            foreground: [255, 255, 255, 255],
        }
    }
}

/// All edits made to one source video.
///
/// Serialised as the project file; `#[serde(default)]` lets files that lack newer fields still
/// load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    /// The source video. Reset to the opened file on load, so a stale stored path is harmless.
    pub source: PathBuf,
    /// Kept part of the source; `None` keeps everything.
    pub trim: Option<Span>,
    /// Source sections removed from the output (may overlap and be in any order).
    pub cuts: Vec<Span>,
    /// Zoom-and-pan effects.
    pub zooms: Vec<Zoom>,
    /// Graphics and text drawn over the video.
    pub annotations: Vec<Annotation>,
    /// Title pages inserted into the timeline.
    pub titles: Vec<TitlePage>,
    /// Playback speed factor (0.25..=4.0); audio keeps its pitch.
    pub speed: f32,
    /// Linear gain applied to the recorded audio (1.0 = unchanged).
    pub volume: f32,
    /// Removes the recorded audio.
    pub mute: bool,
    /// Fade-in length from black and silence at the start of the output, in seconds.
    pub fade_in: f32,
    /// Fade-out length at the end of the output, in seconds.
    pub fade_out: f32,
    /// Optional background music.
    pub music: Option<Music>,
    /// Sections played faster.
    pub speedups: Vec<SpeedUp>,
    /// Logo drawn in a corner of the exported video.
    pub watermark: Option<Watermark>,
}

/// A logo image drawn in a corner of every exported frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    /// The image file (PNG or JPEG; PNG transparency is kept).
    pub path: PathBuf,
    /// Corner of the video it sits in.
    pub corner: crate::settings::WebcamPosition,
    /// Width as a fraction of the video width (0.05..=0.4).
    pub size: f32,
    /// Opacity (0.1..=1).
    pub opacity: f32,
}

impl Default for Watermark {
    /// Bottom-right, 15 % of the video width, slightly see-through.
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            corner: crate::settings::WebcamPosition::BottomRight,
            size: 0.15,
            opacity: 0.8,
        }
    }
}

/// A section played faster in the export (a fast-forward through a slow part).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedUp {
    /// The section, in source seconds.
    pub span: Span,
    /// How many times faster it plays (2, 4 or 8 in the editor).
    pub factor: f32,
}

/// Background music looped under the whole video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Music {
    /// Audio file to play.
    pub path: PathBuf,
    /// Linear gain of the music.
    pub volume: f32,
    /// Lowers the music automatically while someone speaks in the recording.
    pub duck: bool,
}

impl Default for Music {
    /// No file yet, volume 0.35 and ducking enabled.
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            volume: 0.35,
            duck: true,
        }
    }
}

impl Default for Project {
    /// A project without edits: normal speed and volume, no fades and no music.
    fn default() -> Self {
        Self {
            source: PathBuf::new(),
            trim: None,
            cuts: vec![],
            zooms: vec![],
            annotations: vec![],
            titles: vec![],
            music: None,
            speedups: vec![],
            watermark: None,
            speed: 1.0,
            volume: 1.0,
            mute: false,
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }
}

/// One contiguous piece of the output timeline.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// A section of the source video, with the zoom active during it (if any) and how many
    /// times faster it plays (1 normally).
    Source {
        /// The source interval, in seconds.
        span: Span,
        /// The zoom active throughout the piece, if any.
        zoom: Option<Zoom>,
        /// Speed-up factor from [`Project::speedups`] (at least 1); the global
        /// [`Project::speed`] is applied separately.
        speed: f32,
    },
    /// The title page with this index in [`Project::titles`].
    Title(usize),
}

impl Project {
    /// Creates a project without edits for the video at `source`.
    pub fn new(source: PathBuf) -> Self {
        Self {
            source,
            ..Default::default()
        }
    }

    /// Returns the path of the project file for `video` (`<video>.srproj.json`).
    pub fn file_for(video: &Path) -> PathBuf {
        let mut s = video.as_os_str().to_owned();
        s.push(".srproj.json");
        PathBuf::from(s)
    }

    /// Loads the saved project for `video`, or creates an empty one if there is no readable
    /// project file.
    pub fn load_or_new(video: &Path) -> Self {
        std::fs::read_to_string(Self::file_for(video))
            .ok()
            .and_then(|s| serde_json::from_str::<Project>(&s).ok())
            .map(|mut p| {
                p.source = video.to_path_buf();
                p
            })
            .unwrap_or_else(|| Project::new(video.to_path_buf()))
    }

    /// Returns whether the project changes nothing: no trim, cuts, zooms, annotations, title
    /// pages, music, speed-ups or logo, and speed, volume, mute and fades at their defaults.
    pub fn is_empty_edit(&self) -> bool {
        let d = Project::default();
        self.trim.is_none()
            && self.cuts.is_empty()
            && self.zooms.is_empty()
            && self.annotations.is_empty()
            && self.titles.is_empty()
            && self.music.is_none()
            && self.speedups.is_empty()
            && self.watermark.is_none()
            && self.speed == d.speed
            && self.volume == d.volume
            && self.mute == d.mute
            && self.fade_in == d.fade_in
            && self.fade_out == d.fade_out
    }

    /// Writes the project file next to the source video.
    ///
    /// When [`is_empty_edit`](Self::is_empty_edit) holds, the file is deleted instead, so
    /// unedited videos leave no project file behind. Errors are ignored: auto-save is best effort.
    pub fn save(&self) {
        let path = Self::file_for(&self.source);
        if self.is_empty_edit() {
            let _ = std::fs::remove_file(path);
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Returns the trimmed part of a source of `duration` seconds, clamped to the source.
    pub fn trim_span(&self, duration: f64) -> Span {
        let t = self.trim.unwrap_or(Span::new(0.0, duration));
        Span::new(t.start.clamp(0.0, duration), t.end.clamp(0.0, duration))
    }

    /// Returns the source intervals that survive trimming and cuts, in time order.
    ///
    /// Pieces shorter than 40 ms (about a frame) are dropped.
    pub fn kept_spans(&self, duration: f64) -> Vec<Span> {
        let trim = self.trim_span(duration);
        let mut cuts = self.cuts.clone();
        cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut out = vec![];
        // Sweep through the cuts in start order; `cur` is where the next kept span would begin.
        let mut cur = trim.start;
        for c in cuts {
            if c.end <= cur || c.start >= trim.end {
                continue;
            }
            if c.start > cur {
                out.push(Span::new(cur, c.start.min(trim.end)));
            }
            cur = cur.max(c.end);
        }
        if cur < trim.end {
            out.push(Span::new(cur, trim.end));
        }
        out.retain(|s| s.len() > 0.04);
        out
    }

    /// Returns whether source time `t` is removed from the output by trimming or a cut.
    pub fn is_cut(&self, t: f64, duration: f64) -> bool {
        !self.kept_spans(duration).iter().any(|s| s.contains(t))
    }

    /// Returns the output timeline: the kept source spans, split wherever a zoom or speed-up
    /// starts or ends or a title page is inserted, with the title pages in place.
    ///
    /// Each source piece carries the zoom and speed-up factor active in it. A title page goes
    /// before the first source piece starting at or after its time (within 20 ms), or at the end.
    pub fn pieces(&self, duration: f64) -> Vec<Piece> {
        // Times at which a source span must be split.
        let mut bounds: Vec<f64> = vec![];
        for z in &self.zooms {
            bounds.push(z.span.start);
            bounds.push(z.span.end);
        }
        for u in &self.speedups {
            bounds.push(u.span.start);
            bounds.push(u.span.end);
        }
        for t in &self.titles {
            bounds.push(t.at);
        }
        // Split each kept span at the boundaries inside it, ignoring boundaries within 20 ms of
        // each other or of the span's ends to avoid slivers.
        let mut src_pieces = vec![];
        for span in self.kept_spans(duration) {
            let mut points: Vec<f64> = bounds
                .iter()
                .copied()
                .filter(|b| *b > span.start + 0.02 && *b < span.end - 0.02)
                .collect();
            points.sort_by(|a, b| a.total_cmp(b));
            points.dedup_by(|a, b| (*a - *b).abs() < 0.02);
            let mut prev = span.start;
            for p in points.into_iter().chain(std::iter::once(span.end)) {
                // A piece lies entirely inside or outside each zoom and speed-up, so its
                // midpoint decides which ones apply.
                let mid = (prev + p) / 2.0;
                let zoom = self.zooms.iter().find(|z| z.span.contains(mid)).cloned();
                let speed = self
                    .speedups
                    .iter()
                    .find(|u| u.span.contains(mid))
                    .map_or(1.0, |u| u.factor.max(1.0));
                src_pieces.push(Piece::Source {
                    span: Span::new(prev, p),
                    zoom,
                    speed,
                });
                prev = p;
            }
        }

        // Merge the title pages, sorted by time, into the sequence of source pieces.
        let mut titles: Vec<(usize, f64)> = self
            .titles
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.at))
            .collect();
        titles.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut out = vec![];
        let mut ti = 0;
        for piece in src_pieces {
            if let Piece::Source { span, .. } = &piece {
                while ti < titles.len() && titles[ti].1 <= span.start + 0.02 {
                    out.push(Piece::Title(titles[ti].0));
                    ti += 1;
                }
            }
            out.push(piece);
        }
        // Title pages after the last kept piece go at the end.
        while ti < titles.len() {
            out.push(Piece::Title(titles[ti].0));
            ti += 1;
        }
        out
    }

    /// Returns the output duration in seconds before the speed change.
    pub fn output_duration(&self, duration: f64) -> f64 {
        self.pieces(duration)
            .iter()
            .map(|p| match p {
                Piece::Source { span, speed, .. } => span.len() / *speed as f64,
                Piece::Title(i) => self.titles[*i].duration,
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trim and unsorted, partly out-of-range cuts produce the right kept spans.
    #[test]
    fn kept_spans_with_trim_and_cuts() {
        let mut p = Project::new("a.mp4".into());
        p.trim = Some(Span::new(1.0, 9.0));
        p.cuts = vec![
            Span::new(3.0, 4.0),
            Span::new(0.0, 2.0),
            Span::new(8.5, 12.0),
        ];
        let k = p.kept_spans(10.0);
        assert_eq!(k, vec![Span::new(2.0, 3.0), Span::new(4.0, 8.5)]);
        assert!(p.is_cut(3.5, 10.0));
        assert!(!p.is_cut(5.0, 10.0));
    }

    /// Zoom boundaries split the source and a title page at time 0 comes first.
    #[test]
    fn pieces_split_by_zoom_and_titles() {
        let mut p = Project::new("a.mp4".into());
        p.zooms.push(Zoom {
            span: Span::new(2.0, 4.0),
            rect: NRect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 0.5,
            },
        });
        p.titles.push(TitlePage::new(0.0));
        let pieces = p.pieces(6.0);
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces[0], Piece::Title(0));
        assert!(matches!(pieces[2], Piece::Source { zoom: Some(_), .. }));
        assert!((p.output_duration(6.0) - 9.0).abs() < 1e-9);
    }

    /// Clicks are grouped by time and place, framed at 2× and kept apart and inside the video.
    #[test]
    fn auto_zoom_groups_clicks() {
        let c = |t: f64, x: f32, y: f32| Click { t, x, y };
        let clicks = [
            c(2.0, 0.2, 0.3),
            c(3.0, 0.25, 0.35), // same place soon after: same zoom
            c(10.0, 0.8, 0.8),  // later: a second zoom
            c(10.5, 0.1, 0.1),  // far away: a third zoom
            c(30.0, 1.5, 0.5),  // outside the video: ignored
        ];
        let z = auto_zooms(&clicks, 12.0);
        assert_eq!(z.len(), 3);
        assert!((z[0].span.start - 1.3).abs() < 1e-9 && (z[0].span.end - 4.8).abs() < 1e-9);
        // 2× zoom keeping the frame shape, inside the frame.
        assert_eq!((z[0].rect.w, z[0].rect.h), (0.5, 0.5));
        assert_eq!((z[1].rect.x, z[1].rect.y), (0.5, 0.5));
        // A group too short to keep does not shorten the zoom before it.
        let z2 = auto_zooms(&[c(1.0, 0.2, 0.2), c(2.5, 0.9, 0.9)], 2.7);
        assert_eq!(z2.len(), 1);
        assert!((z2[0].span.end - 2.7).abs() < 1e-9);
        // No overlap and clipped to the video length.
        assert!(z[2].span.start >= z[1].span.end);
        assert!(z[2].span.end <= 12.0);
    }

    /// The crop stays still until the pointer moves away, then follows it gradually.
    #[test]
    fn follow_path_follows_the_pointer_smoothly() {
        let c = |t: f64, x: f32, y: f32| Click { t, x, y };
        // Pointer left, then far right at 2 s.
        let p = [c(0.0, 0.1, 0.5), c(2.0, 0.9, 0.5)];
        let w = 0.32; // a 9:16 frame from 16:9
        let path = follow_path(&p, 5.0, w, 1.0);
        assert_eq!(path.len(), 151);
        // Starts as far left as possible (frame stays inside the source).
        assert!((follow_at(&path, 0.0).0 - w / 2.0).abs() < 1e-3);
        // Has not moved before the pointer does, moves gradually after.
        assert!((follow_at(&path, 1.9).0 - w / 2.0).abs() < 1e-3);
        let mid = follow_at(&path, 2.2).0;
        assert!(mid > w / 2.0 + 0.05 && mid < 0.9 - w * 0.3);
        // Ends with the pointer inside the frame's middle part.
        let end = follow_at(&path, 5.0).0;
        assert!((end - (0.9 - w * 0.3)).abs() < 0.01, "{end}");
        // Full height: never moves vertically.
        assert!(path.iter().all(|c| (c.1 - 0.5).abs() < 1e-6));
    }

    /// A zoom starts and ends at the whole frame and is fully zoomed in the middle.
    #[test]
    fn zoom_eases_in_and_out() {
        let z = Zoom {
            span: Span::new(1.0, 5.0),
            rect: NRect {
                x: 0.5,
                y: 0.5,
                w: 0.5,
                h: 0.5,
            },
        };
        assert_eq!(z.rect_at(1.0).w, 1.0); // starts from the whole frame
        assert_eq!(z.rect_at(3.0), z.rect); // fully zoomed in the middle
        let mid = z.rect_at(1.2);
        assert!(mid.w < 1.0 && mid.w > 0.5);
        assert_eq!(z.rect_at(5.0).w, 1.0); // and back out at the end
    }

    /// Clip settings and music alone are edits worth saving.
    #[test]
    fn clip_settings_and_music_count_as_edits() {
        assert!(Project::new("a.mp4".into()).is_empty_edit());
        let mut p = Project::new("a.mp4".into());
        p.speed = 1.5;
        assert!(!p.is_empty_edit());
        let mut p = Project::new("a.mp4".into());
        p.music = Some(Music::default());
        assert!(!p.is_empty_edit());
        let mut p = Project::new("a.mp4".into());
        p.fade_out = 2.0;
        assert!(!p.is_empty_edit());
    }

    /// A sped-up section is split out of the source and shortens the output.
    #[test]
    fn speedup_sections_shorten_the_output() {
        let mut p = Project::new("a.mp4".into());
        p.speedups.push(SpeedUp {
            span: Span::new(2.0, 6.0),
            factor: 4.0,
        });
        let pieces = p.pieces(10.0);
        assert_eq!(pieces.len(), 3);
        assert!(matches!(pieces[1], Piece::Source { speed, .. } if speed == 4.0));
        // 2 s + 4 s at 4x (1 s) + 4 s = 7 s.
        assert!((p.output_duration(10.0) - 7.0).abs() < 1e-9);
    }

    /// Pixel conversion yields even values that fit inside an odd-sized frame.
    #[test]
    fn nrect_pixels_even() {
        let r = NRect {
            x: 0.1,
            y: 0.1,
            w: 0.5,
            h: 0.5,
        };
        let (x, y, w, h) = r.to_pixels_even(1001, 777);
        assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0);
        assert!(x + w <= 1001 && y + h <= 777);
    }
}
