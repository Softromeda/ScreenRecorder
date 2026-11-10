// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Render engine used by the editor's export and by the conversion tools.
//!
//! A [`RenderJob`] describes the output as a sequence of [`Part`]s (source clips and still
//! images) plus overlays, speed, volume, fades, background music and an optional pointer-following
//! crop. [`render`] decodes the clips with Media Foundation, applies overlays (annotations, blur),
//! zoom, cropping and letterboxing, speed and fades in Rust, and encodes H.264 + AAC (MP4) or
//! writes an animated GIF, reporting progress through a [`Job`]. [`decode_audio`] reads a whole
//! audio track for music, pause detection and audio extraction.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use super::project::{NRect, Span, Zoom};
use crate::job::Job;
use crate::media::{audio, convert, gifenc::GifWriter, mf};

/// Something drawn on the source frames while its span is active (times are source times;
/// coordinates are relative to the source frame).
pub enum Overlay {
    /// Straight-alpha BGRA image at source resolution (rendered annotations).
    Image {
        /// Source time range during which the image is drawn.
        span: Span,
        /// Pixels, `width * height * 4` bytes of the source frame; images of any other size
        /// are ignored.
        bgra: Vec<u8>,
    },
    /// A blurred rectangle, for hiding sensitive information.
    Blur {
        /// Source time range during which the rectangle is blurred.
        span: Span,
        /// The area to blur, in normalised source coordinates.
        rect: NRect,
    },
}

/// One consecutive piece of the output video.
pub enum Part {
    /// A section of `sources[source]` (source times), optionally zoomed.
    Clip {
        /// Index into [`RenderJob::sources`].
        source: usize,
        /// Start of the section in source seconds.
        start: f64,
        /// End of the section in source seconds (exclusive).
        end: f64,
        /// Zoom applied to this section, evaluated at source times.
        zoom: Option<Zoom>,
        /// How many times faster this part plays (1 normally). Faster parts are muted and get
        /// the fast-forward badge.
        speed: f64,
    },
    /// A still image, e.g. a title page.
    Still {
        /// Opaque BGRA pixels at the job's `width` × `height`.
        bgra: Vec<u8>,
        /// How long it is shown, in seconds before the job-wide speed change.
        duration: f64,
    },
}

/// Output format of a render.
pub enum Output {
    /// H.264 + AAC in MP4.
    Mp4 {
        /// Quality setting, 1..=100 (see [`video_bitrate`](crate::media::video_bitrate)).
        quality: u32,
    },
    /// Silent animated GIF.
    Gif {
        /// Frame rate of the GIF (independent of [`RenderJob::fps`]).
        fps: f64,
        /// Maximum width in pixels; wider outputs are scaled down, keeping the aspect ratio.
        max_width: u32,
    },
}

/// Everything needed to render one output file.
pub struct RenderJob {
    /// Source video files referenced by [`Part::Clip`].
    pub sources: Vec<PathBuf>,
    /// The output, in order.
    pub parts: Vec<Part>,
    /// Output width in pixels (rounded down to even). For GIF it is reduced to `max_width`.
    pub width: u32,
    /// Output height in pixels (rounded down to even).
    pub height: u32,
    /// Output frame rate for MP4 (GIF uses its own rate).
    pub fps: f64,
    /// Annotations and blurs applied to clip frames.
    pub overlays: Vec<Overlay>,
    /// Playback speed (0.25–4); audio is time-stretched with pitch preserved.
    pub speed: f64,
    /// Volume multiplier for the source audio.
    pub volume: f32,
    /// Drop the source audio (background music is still mixed in).
    pub mute: bool,
    /// Fade-in length in seconds of output time (video to black and audio), 0 for none.
    pub fade_in: f64,
    /// Fade-out length in seconds of output time, 0 for none.
    pub fade_out: f64,
    /// Container and encoding.
    pub output: Output,
    /// Vertical / square export: centre of the frame that follows the pointer (source fractions,
    /// one entry every 1/30 s of source time).
    pub follow: Option<Vec<(f32, f32)>>,
    /// Background music, mixed under the whole output. In a `RefCell` because the mixer keeps
    /// ducking state while the job itself is shared immutably.
    pub music: Option<std::cell::RefCell<crate::media::voice::MusicMixer>>,
    /// Fast-forward badges shown on sped-up parts: (speed, width, height, straight-alpha BGRA).
    pub badges: Vec<(f64, usize, usize, Vec<u8>)>,
    /// Logo drawn in a corner of every output frame.
    pub logo: Option<Logo>,
}

/// A logo for [`RenderJob::logo`], scaled to the output size when the render starts.
pub struct Logo {
    /// The image as loaded (straight-alpha RGBA).
    pub image: image::RgbaImage,
    /// Corner of the frame.
    pub corner: crate::settings::WebcamPosition,
    /// Width as a fraction of the output width.
    pub size: f32,
    /// Opacity (0..=1).
    pub opacity: f32,
}

impl Logo {
    /// The logo at its size in a `w` × `h` output: position, size and straight-alpha BGRA pixels
    /// (opacity applied). `None` if it would not fit.
    fn prepare(&self, w: usize, h: usize) -> Option<(usize, usize, usize, usize, Vec<u8>)> {
        use crate::settings::WebcamPosition as C;
        let (iw, ih) = self.image.dimensions();
        if iw == 0 || ih == 0 {
            return None;
        }
        // Width from the size fraction (kept within 2–60 % of the frame), height from the
        // image's aspect ratio; the margin scales with the frame height like the badge's.
        let lw = ((w as f32 * self.size.clamp(0.02, 0.6)).round() as u32).max(4);
        let lh = ((lw as u64 * ih as u64 / iw as u64) as u32).max(1);
        let margin = (h / 30).max(6) as u32;
        if lw + 2 * margin > w as u32 || lh + 2 * margin > h as u32 {
            return None;
        }
        let scaled =
            image::imageops::resize(&self.image, lw, lh, image::imageops::FilterType::Triangle);
        let op = self.opacity.clamp(0.0, 1.0);
        let mut bgra = scaled.into_raw();
        // RGBA to BGRA, with the opacity folded into the alpha channel.
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = (px[3] as f32 * op).round() as u8;
        }
        let (x, y) = match self.corner {
            C::TopLeft => (margin, margin),
            C::TopRight => (w as u32 - lw - margin, margin),
            C::BottomLeft => (margin, h as u32 - lh - margin),
            C::BottomRight => (w as u32 - lw - margin, h as u32 - lh - margin),
        };
        Some((x as usize, y as usize, lw as usize, lh as usize, bgra))
    }
}

/// Alpha-blends a straight-alpha BGRA image (`bw` × `bh`) onto an opaque BGRA frame `w` pixels
/// wide, with its top-left corner at (`x0`, `y0`). The image must fit inside the frame.
fn blend_bgra(img: &mut [u8], w: usize, x0: usize, y0: usize, bw: usize, bh: usize, src: &[u8]) {
    for y in 0..bh {
        for x in 0..bw {
            let s = (y * bw + x) * 4;
            let a = src[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((y0 + y) * w + x0 + x) * 4;
            for c in 0..3 {
                img[d + c] = ((src[s + c] as u32 * a + img[d + c] as u32 * (255 - a)) / 255) as u8;
            }
        }
    }
}

impl RenderJob {
    /// Output length of one part in seconds, after the speed change.
    fn part_duration(&self, p: &Part) -> f64 {
        match p {
            // A part's own speed never slows it down; values below 1 count as 1.
            Part::Clip {
                start, end, speed, ..
            } => (end - start).max(0.0) / (self.speed * speed.max(1.0)),
            Part::Still { duration, .. } => *duration / self.speed,
        }
    }

    /// Output length of the whole job in seconds, after the speed change.
    pub fn total_duration(&self) -> f64 {
        self.parts.iter().map(|p| self.part_duration(p)).sum()
    }
}

/// The open output file.
enum Sink {
    /// MP4 writer plus a reusable NV12 conversion buffer.
    Mp4 {
        /// The Media Foundation sink writer (encodes H.264 and AAC).
        writer: mf::Writer,
        /// Scratch buffer each BGRA frame is converted into before encoding, kept to avoid an
        /// allocation per frame.
        nv12: Vec<u8>,
    },
    /// Animated GIF writer (expects RGBA frames).
    Gif(GifWriter),
}

/// Pulls stereo 48 kHz audio for one clip, converted and time-stretched, aligned so that
/// sample 0 is the clip start. Volume and fades are applied later by [`write_audio`].
struct ClipAudio {
    /// The clip's audio decoder, or `None` when the source has no usable audio (the clip is then
    /// silent).
    reader: Option<mf::AudioReader>,
    /// Converts the source's rate and channel layout to stereo at [`audio::RATE`].
    conv: audio::Converter,
    /// Time-stretches by the job-wide speed with pitch preserved.
    stretch: audio::Stretcher,
    /// Clip start in source seconds.
    start: f64,
    /// Converted frames still to drop because they precede the clip start; `usize::MAX` once
    /// the leading audio has been trimmed.
    skip_frames: usize,
    /// Source frames (at 48 kHz) remaining until the clip end.
    src_frames_left: usize,
    /// Stretched interleaved stereo output not yet taken.
    out: Vec<f32>,
    /// No more input will be read (end of clip or of the stream).
    done: bool,
}

impl ClipAudio {
    /// Opens the audio of `source` for the source range `start..end` played at `speed`.
    ///
    /// Errors opening or seeking are not fatal: the clip then produces silence.
    fn new(source: &Path, start: f64, end: f64, speed: f64) -> Self {
        let reader = mf::AudioReader::open(source)
            .ok()
            .and_then(|mut r| r.seek(start).ok().map(|_| r));
        let (rate, ch) = reader
            .as_ref()
            .map(|r| (r.rate, r.channels))
            .unwrap_or((audio::RATE, 2));
        Self {
            reader,
            conv: audio::Converter::new(rate, ch),
            stretch: audio::Stretcher::new(speed),
            start,
            skip_frames: 0,
            src_frames_left: ((end - start) * audio::RATE as f64) as usize,
            out: vec![],
            done: false,
        }
    }

    /// Decodes until at least `frames` output frames are buffered or the clip has ended.
    fn fill(&mut self, frames: usize) -> Result<()> {
        while self.out.len() < frames * 2 && !self.done {
            let Some(reader) = self.reader.as_mut() else {
                self.done = true;
                break;
            };
            let Some((t, chunk)) = reader.read_chunk()? else {
                self.done = true;
                self.stretch.push(&[], &mut self.out);
                break;
            };
            let mut conv = vec![];
            self.conv.push(&chunk, &mut conv);
            // Drop audio that lies before the clip start (seeking is coarse); the amount is
            // determined once, from the first chunk's timestamp.
            if self.skip_frames == 0 && t < self.start {
                self.skip_frames = ((self.start - t) * audio::RATE as f64) as usize;
            }
            let mut slice = &conv[..];
            if self.skip_frames > 0 {
                let n = self.skip_frames.min(slice.len() / 2);
                slice = &slice[n * 2..];
                self.skip_frames -= n;
                if self.skip_frames == 0 {
                    self.skip_frames = usize::MAX; // trimmed; never again
                }
            }
            // Stop at the clip end.
            let n = self.src_frames_left.min(slice.len() / 2);
            self.src_frames_left -= n;
            self.stretch.push(&slice[..n * 2], &mut self.out);
            if self.src_frames_left == 0 {
                self.done = true;
            }
        }
        Ok(())
    }

    /// Removes and returns exactly `frames` frames of buffered output, padding with silence.
    fn take(&mut self, frames: usize) -> Vec<f32> {
        let n = (frames * 2).min(self.out.len());
        let mut v: Vec<f32> = self.out.drain(..n).collect();
        v.resize(frames * 2, 0.0); // silence if the source ran short
        v
    }
}

/// Renders `job` into the file `out`, reporting progress (0–0.99) through `progress`.
///
/// Runs on the calling thread and can take a long time. Video is produced frame by frame at the
/// output rate; for each output frame the source frame shown at that time is chosen, so speed
/// changes drop or repeat frames. Audio is written alongside, slightly ahead of the video.
/// Fails if a source cannot be opened or decoded, or the output cannot be written.
pub fn render(job: &RenderJob, out: &Path, progress: &Job) -> Result<()> {
    mf::init()?;
    let total = job.total_duration().max(0.001);
    let infos = job
        .sources
        .iter()
        .map(|s| mf::probe(s))
        .collect::<Result<Vec<_>>>()?;

    // Output size and rate. GIFs are scaled down to the maximum width and have no audio; MP4
    // has audio when any source has some (and it is not muted) or music is added.
    let (mut w, mut h) = (job.width as usize & !1, job.height as usize & !1);
    let (fps, with_audio) = match job.output {
        Output::Mp4 { .. } => (
            job.fps,
            (!job.mute && infos.iter().any(|i| i.has_audio)) || job.music.is_some(),
        ),
        Output::Gif { fps, max_width } => {
            if w > max_width as usize {
                h = ((h as f64 * max_width as f64 / w as f64) as usize).max(2) & !1;
                w = max_width as usize & !1;
            }
            (fps, false)
        }
    };
    let mut sink = match job.output {
        Output::Mp4 { quality } => Sink::Mp4 {
            writer: mf::Writer::create(
                out,
                mf::WriterOptions {
                    video: Some(mf::VideoInput::Encode(mf::VideoSpec {
                        width: w as u32,
                        height: h as u32,
                        fps,
                        bitrate: crate::media::video_bitrate(w as u32, h as u32, fps, quality),
                    })),
                    audio: with_audio.then_some(mf::AudioCodec::Aac),
                    hardware: true,
                    fragmented: false,
                },
            )?,
            nv12: vec![],
        },
        Output::Gif { .. } => Sink::Gif(GifWriter::create(out, w as u32, h as u32)?),
    };

    let frame_dur = 1.0 / fps;
    let mut out_frames: u64 = 0; // video frames written so far
    let mut audio_frames: usize = 0; // audio frames (48 kHz stereo pairs) written so far

    // The logo at this output's size (GIFs may be smaller than the job's size). Prepared once
    // here rather than per frame because scaling the image is comparatively expensive.
    let logo = job.logo.as_ref().and_then(|l| l.prepare(w, h));
    // Writes one finished BGRA frame at output time `t_out`: draws the logo, then applies the
    // video fade (sampled at the frame's middle) so the logo fades with the picture.
    let emit = |sink: &mut Sink, mut img: Vec<u8>, t_out: f64| -> Result<()> {
        if let Some((x, y, lw, lh, px)) = &logo {
            blend_bgra(&mut img, w, *x, *y, *lw, *lh, px);
        }
        let fade = audio::fade_gain(t_out + frame_dur / 2.0, total, job.fade_in, job.fade_out);
        convert::fade(&mut img, fade);
        match sink {
            Sink::Mp4 { writer, nv12 } => {
                convert::bgra_to_nv12(&img, w * 4, w, h, nv12);
                writer.write_nv12(nv12, t_out, frame_dur)
            }
            Sink::Gif(g) => {
                // The GIF writer expects RGBA.
                convert::swap_red_blue(&mut img);
                g.add_rgba(img, frame_dur)
            }
        }
    };

    // Parts are laid end to end; each starts at a whole output frame so timing never drifts.
    for part in &job.parts {
        let part_start = out_frames as f64 * frame_dur;
        let n = (job.part_duration(part) * fps).round().max(1.0) as u64;
        match part {
            // A still is given at the job size; scale it if the output was reduced (GIF).
            Part::Still { bgra, .. } => {
                let (sw, sh) = (job.width as usize, job.height as usize);
                let img = if (sw, sh) == (w, h) {
                    bgra.clone()
                } else {
                    convert::resize(bgra, sw, sh, w, h)
                };
                // Stills are silent apart from music; audio is kept half a second ahead of the
                // video so the writer can interleave the streams.
                for k in 0..n {
                    let t = part_start + k as f64 * frame_dur;
                    emit(&mut sink, img.clone(), t)?;
                    write_audio(&mut sink, &mut audio_frames, t + 0.5, None, job)?;
                }
            }
            Part::Clip {
                source,
                start,
                end,
                zoom,
                speed,
            } => {
                let speed = speed.max(1.0);
                // Decode from the key frame before the clip start, keeping the current frame and
                // the next one to decide which is shown at each output time.
                let path = &job.sources[*source];
                let mut reader = mf::VideoReader::open(path, true)?;
                reader.seek(*start)?;
                // Sped-up parts are silent (sound at 4x or 8x is just noise).
                let mut clip_audio = (with_audio && speed == 1.0)
                    .then(|| ClipAudio::new(path, *start, *end, job.speed));
                let mut cur: Option<mf::Frame> = None;
                let mut next: Option<mf::Frame> = reader.read_frame()?;
                for k in 0..n {
                    // Each output frame advances the source by `frame_dur` scaled by both the
                    // job-wide speed and this part's fast-forward factor.
                    let t_src = start + k as f64 * frame_dur * job.speed * speed;
                    // Advance to the frame shown at t_src (the last one starting at or before
                    // it); before the first frame, use the first frame. The 0.1 ms tolerance
                    // absorbs rounding in frame timestamps so exact matches are not missed.
                    while next.as_ref().is_some_and(|f| f.time <= t_src + 1e-4) {
                        cur = next.take();
                        next = reader.read_frame()?;
                    }
                    let frame = match cur.as_ref().or(next.as_ref()) {
                        Some(f) => f,
                        None => break,
                    };
                    // Zoom rectangle and pointer-follow centre at this source time.
                    let rect = zoom.as_ref().map(|z| z.rect_at(t_src));
                    let centre = job
                        .follow
                        .as_ref()
                        .map(|p| super::project::follow_at(p, t_src));
                    let mut img = process_frame(frame, t_src, rect, centre, job, w, h);
                    if speed > 1.0 {
                        draw_badge(&mut img, w, h, job, speed);
                    }
                    let t = part_start + k as f64 * frame_dur;
                    emit(&mut sink, img, t)?;
                    write_audio(
                        &mut sink,
                        &mut audio_frames,
                        t + 0.5,
                        clip_audio.as_mut(),
                        job,
                    )?;
                    progress.set_progress((t / total) as f32 * 0.98, crate::i18n::tr("Exporting…"));
                }
                // Write this clip's remaining audio up to the end of its video.
                let end_t = part_start + n as f64 * frame_dur;
                write_audio(
                    &mut sink,
                    &mut audio_frames,
                    end_t,
                    clip_audio.as_mut(),
                    job,
                )?;
            }
        }
        out_frames += n;
    }

    progress.set_progress(0.99, crate::i18n::tr("Finishing…"));
    match sink {
        Sink::Mp4 { writer, .. } => writer.finish(),
        Sink::Gif(g) => g.finish(),
    }
}

/// Writes output audio from the current position (`written`, in frames) up to output time
/// `until` in seconds, capped at the job's length; does nothing for GIF output.
///
/// Samples come from `clip` (silence without a clip or when muted), then get the volume and
/// the audio fades, and finally the background music is mixed in.
fn write_audio(
    sink: &mut Sink,
    written: &mut usize,
    until: f64,
    clip: Option<&mut ClipAudio>,
    job: &RenderJob,
) -> Result<()> {
    let Sink::Mp4 { writer, .. } = sink else {
        return Ok(());
    };
    if job.mute && job.music.is_none() {
        return Ok(());
    }
    // Positions are tracked in whole audio frames from the start of the output, so rounding
    // never accumulates across calls.
    let target = (until.min(job.total_duration()) * audio::RATE as f64) as usize;
    if target <= *written {
        return Ok(());
    }
    let frames = target - *written;
    let mut buf = match clip {
        Some(c) if !job.mute => {
            c.fill(frames)?;
            c.take(frames)
        }
        _ => vec![0.0; frames * 2],
    };
    // Volume and fade in / fade out, by output time of each frame.
    let total = job.total_duration();
    for (i, s) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let t = (*written + i) as f64 / audio::RATE as f64;
        let g = job.volume * audio::fade_gain(t, total, job.fade_in, job.fade_out);
        s[0] *= g;
        s[1] *= g;
    }
    if let Some(m) = &job.music {
        let total_frames = (total * audio::RATE as f64) as usize;
        m.borrow_mut().mix_into(&mut buf, *written, total_frames);
    }
    writer.write_audio(&audio::to_i16(&buf))?;
    *written = target;
    Ok(())
}

/// Draws the fast-forward badge for `speed` in the top-right corner of a `w` × `h` BGRA frame.
fn draw_badge(img: &mut [u8], w: usize, h: usize, job: &RenderJob, speed: f64) {
    let Some((_, bw, bh, badge)) = job.badges.iter().find(|b| (b.0 - speed).abs() < 1e-6) else {
        return;
    };
    let margin = (h / 30).max(6);
    if *bw + margin > w || *bh + margin > h {
        return;
    }
    blend_bgra(img, w, w - bw - margin, margin, *bw, *bh, badge);
}

/// Turns one decoded source frame into a `w`×`h` output frame.
///
/// Applies the overlays active at `t_src`, crops to the zoom rectangle (normalised source
/// coordinates), cuts out the pointer-following frame of the output's shape centred on `follow`
/// (normalised source coordinates) when given, and letterboxes the result to the output size.
fn process_frame(
    f: &mf::Frame,
    t_src: f64,
    zoom: Option<NRect>,
    follow: Option<(f32, f32)>,
    job: &RenderJob,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let (sw, sh) = (f.width as usize, f.height as usize);
    // Overlays whose span contains this source time.
    let active: Vec<&Overlay> = job
        .overlays
        .iter()
        .filter(|o| match o {
            Overlay::Image { span, .. } | Overlay::Blur { span, .. } => span.contains(t_src),
        })
        .collect();
    let mut img = if active.is_empty() && zoom.is_none() && follow.is_none() {
        // Fast path: no copy needed before scaling.
        return convert::letterbox(&f.bgra, sw, sh, w, h);
    } else {
        f.bgra.clone()
    };
    for o in active {
        match o {
            // Blur strength scales with the rectangle size, within 2–24 pixels.
            Overlay::Blur { rect, .. } => {
                let (x, y, rw, rh) = rect.to_pixels_even(sw as u32, sh as u32);
                let radius = ((rw.min(rh) / 12).clamp(2, 24)) as usize;
                convert::blur_rect(
                    &mut img,
                    sw,
                    sh,
                    x as usize,
                    y as usize,
                    rw as usize,
                    rh as usize,
                    radius,
                );
            }
            Overlay::Image { bgra, .. } => {
                if bgra.len() == img.len() {
                    convert::blend_over(&mut img, bgra);
                }
            }
        }
    }
    // Zoom: crop to the visible part of the source; `area` remembers where it lies in
    // normalised source coordinates for the pointer-follow step.
    let full = NRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
    let (img, iw, ih, area) = match zoom {
        Some(z) => {
            let (x, y, zw, zh) = z.to_pixels_even(sw as u32, sh as u32);
            let (zw, zh) = (zw as usize, zh as usize);
            let cropped = convert::crop(&img, sw, x as usize, y as usize, zw, zh);
            (cropped, zw, zh, z)
        }
        None => (img, sw, sh, full),
    };
    // Vertical / square: a frame of the output's shape, following the pointer, cut out of the
    // visible part. It is as large as fits, centred on the pointer and kept inside the image.
    if let Some((cx, cy)) = follow {
        // Largest crop of the output's aspect ratio that fits in the visible image; then map
        // the pointer from normalised source coordinates into it and clamp the crop's origin.
        let aspect = w as f64 / h as f64;
        let (cw, ch) = if iw as f64 / ih as f64 > aspect {
            (((ih as f64 * aspect).round() as usize).clamp(2, iw), ih)
        } else {
            (iw, ((iw as f64 / aspect).round() as usize).clamp(2, ih))
        };
        let u = ((cx - area.x) / area.w.max(1e-3)).clamp(0.0, 1.0) * iw as f32;
        let v = ((cy - area.y) / area.h.max(1e-3)).clamp(0.0, 1.0) * ih as f32;
        let x0 = (u - cw as f32 / 2.0).clamp(0.0, (iw - cw) as f32) as usize;
        let y0 = (v - ch as f32 / 2.0).clamp(0.0, (ih - ch) as f32) as usize;
        let cut = convert::crop(&img, iw, x0, y0, cw, ch);
        return convert::letterbox(&cut, cw, ch, w, h);
    }
    convert::letterbox(&img, iw, ih, w, h)
}

/// Returns the output length of `job` in seconds, after the speed change.
#[allow(dead_code)]
pub fn output_length(job: &RenderJob) -> f64 {
    job.total_duration()
}

/// Decodes the whole first audio track of a file to interleaved stereo at
/// [`RATE`](audio::RATE).
///
/// Used for background music, pause detection and the Extract Audio tool. Progress is reported
/// from 0 to 0.8. Fails if the file has no audio track or cannot be decoded.
pub fn decode_audio(path: &Path, progress: &Job) -> Result<Vec<f32>> {
    mf::init()?;
    let info = mf::probe(path)?;
    let mut reader =
        mf::AudioReader::open(path).map_err(|_| anyhow!("The file has no audio track"))?;
    let mut conv = audio::Converter::new(reader.rate, reader.channels);
    let mut out = vec![];
    while let Some((t, chunk)) = reader.read_chunk()? {
        conv.push(&chunk, &mut out);
        if info.duration > 0.0 {
            progress.set_progress(
                (t / info.duration) as f32 * 0.8,
                crate::i18n::tr("Decoding audio…"),
            );
        }
    }
    Ok(out)
}
