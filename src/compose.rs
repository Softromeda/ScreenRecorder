// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Turns the raw material of a recording session into the final file.
//!
//! [`finalize`] is called after [`crate::recorder::Recorder::stop`] (or when recovering a crashed
//! session); it runs [`compose`] and then removes the session directory. By output format:
//!
//! * MP4: the H.264 video is copied without re-encoding (`transcode` is the fallback) and the
//!   mixed microphone / speaker audio (plus click sounds, when enabled) is added as AAC.
//! * GIF: the video is decoded and written as an animated GIF ([`write_gif`]).
//! * MP3: the mixed audio only ([`write_audio_file`], also used by the conversion tools).
//!
//! Progress is reported to an optional [`Job`] for the UI.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::job::Job;
use crate::media::{audio, convert, gifenc::GifWriter, mf};
use crate::recorder::{FinishedSession, SessionManifest};
use crate::settings::{OutputFormat, Settings};

/// Size of the file at `p` in bytes, or 0 if it does not exist or cannot be read.
fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Reports progress `p` (0..=1) with status message `msg` to `job`, if there is one.
fn progress(job: Option<&Job>, p: f32, msg: &str) {
    if let Some(j) = job {
        j.set_progress(p, msg);
    }
}

/// Loads, cleans up and mixes the session's audio tracks into interleaved stereo 48 kHz samples.
///
/// The microphone track gets noise reduction and levelling, the speaker track an automatic
/// boost, each when enabled in the session's settings. When `duration` is positive the result
/// is padded with silence or cut to exactly that many seconds. Returns `None` if no usable track
/// was recorded; files of 1 KiB or less are treated as empty (a WAV header only).
fn mixed_audio(dir: &Path, m: &SessionManifest, duration: f64) -> Result<Option<Vec<f32>>> {
    let mut tracks = vec![];
    let mic = SessionManifest::mic_path(dir);
    if m.has_mic && file_len(&mic) > 1024 {
        let mut t = audio::to_stereo_48k(&audio::read_wav(&mic)?);
        if m.settings.mic_noise_reduction {
            let (over, max_db) = m.settings.noise_strength.params();
            crate::media::voice::denoise(&mut t, over, max_db);
        }
        if m.settings.mic_leveling {
            crate::media::voice::level(&mut t);
        }
        tracks.push(t);
    }
    let spk = SessionManifest::speakers_path(dir);
    if m.has_speakers && file_len(&spk) > 1024 {
        let mut t = audio::to_stereo_48k(&audio::read_wav(&spk)?);
        if m.settings.auto_boost_speakers {
            audio::auto_boost(&mut t);
        }
        tracks.push(t);
    }
    if m.settings.click_sound {
        if let Some(t) = click_track(dir, duration) {
            tracks.push(t);
        }
    }
    if tracks.is_empty() {
        return Ok(None);
    }
    let mut mixed = audio::mix(&tracks);
    if duration > 0.0 {
        // Match the video length exactly (2 interleaved samples per stereo frame).
        mixed.resize((duration * audio::RATE as f64) as usize * 2, 0.0);
    }
    Ok(Some(mixed))
}

/// Builds the click-sound track from the session's `clicks.json` (left clicks inside the
/// recorded area, on the recording's timeline). It lasts `duration` seconds, or until just after
/// the last click when the duration is unknown. Returns `None` if no clicks were recorded.
fn click_track(dir: &Path, duration: f64) -> Option<Vec<f32>> {
    let json = std::fs::read_to_string(dir.join("clicks.json")).ok()?;
    let clicks: Vec<crate::editor::project::Click> = serde_json::from_str(&json).ok()?;
    if clicks.is_empty() {
        return None;
    }
    let times: Vec<f64> = clicks.iter().map(|c| c.t).collect();
    let secs = if duration > 0.0 {
        duration
    } else {
        times.iter().cloned().fold(0.0, f64::max) + 0.1
    };
    Some(audio::click_track(
        &times,
        (secs * audio::RATE as f64) as usize,
    ))
}

/// Produces the final output file for `session` and returns its path.
///
/// The session directory is left untouched; see [`finalize`] for the variant that cleans up.
/// Fails if nothing usable was recorded for the chosen format or encoding fails.
pub fn compose(session: &FinishedSession, job: Option<&Job>) -> Result<PathBuf> {
    let dir = &session.dir;
    let m = &session.manifest;
    let out = m.output.clone();
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let video = SessionManifest::video_path(dir);
    // A failed encoder can leave an empty file behind.
    let has_video = m.has_video && file_len(&video) > 1024;

    // Length of the recording: the manifest's active duration, or for a crashed session (where
    // it is 0) the length of the recovered video.
    progress(job, 0.02, "Preparing audio…");
    let duration = if m.duration > 0.0 {
        m.duration
    } else if has_video {
        mf::probe(&video).map(|i| i.duration).unwrap_or(0.0)
    } else {
        0.0
    };
    let wants_audio = m.settings.format.has_audio();
    let audio = if wants_audio {
        mixed_audio(dir, m, duration)?
    } else {
        None
    };

    // Write the output in the chosen format.
    match m.settings.format {
        OutputFormat::Mp3 => {
            let samples = audio.ok_or_else(|| anyhow!("No audio was recorded."))?;
            write_audio_file(&out, mf::AudioCodec::Mp3, &samples, job)?;
        }
        OutputFormat::Gif => {
            if !has_video {
                return Err(no_video_error(m));
            }
            // GIFs are capped at 15 fps to keep the file size reasonable.
            write_gif(&video, &out, m.settings.fps.min(15) as f64, job)?;
        }
        OutputFormat::Mp4 => {
            if !has_video {
                return Err(no_video_error(m));
            }
            // Copying the stream is fast and lossless; re-encode only if that fails.
            if let Err(e) = remux(&video, &out, audio.as_deref(), duration, job) {
                log::warn!("copying the video failed ({e:#}); re-encoding instead");
                transcode(&video, &out, audio.as_deref(), &m.settings, job)?;
            }
        }
    }
    if !out.is_file() {
        return Err(anyhow!("the output file was not created"));
    }
    Ok(out)
}

/// The error for a session without usable video: either video capture failed (point the user to
/// the log) or the session never had video.
fn no_video_error(m: &SessionManifest) -> anyhow::Error {
    if m.has_video {
        anyhow!(
            "The screen video could not be recorded. Details are in the log file \
             (Options → General → Open log file)."
        )
    } else {
        anyhow!("Nothing was recorded.")
    }
}

/// Copies the H.264 stream from the fragmented session video into a normal MP4 without
/// re-encoding, adding `audio` (interleaved stereo 48 kHz) as AAC.
///
/// `duration` (seconds) is used only for progress reporting. Fails if the video has no samples.
fn remux(
    video: &Path,
    out: &Path,
    audio: Option<&[f32]>,
    duration: f64,
    job: Option<&Job>,
) -> Result<()> {
    let mut reader = mf::VideoReader::open(video, false)?;
    let native = reader.native_type()?;
    let mut writer = mf::Writer::create(
        out,
        mf::WriterOptions {
            video: Some(mf::VideoInput::Passthrough(&native)),
            audio: audio.map(|_| mf::AudioCodec::Aac),
            hardware: true,
            fragmented: false,
        },
    )?;
    let pcm = audio.map(audio::to_i16);
    let mut audio_pos = 0usize; // in samples (interleaved)
    let mut count = 0u64;
    while let Some(sample) = reader.read_compressed()? {
        // SAFETY: `sample` is a valid IMFSample returned by the reader; reading its time has no
        // other preconditions.
        let t = unsafe { sample.GetSampleTime().unwrap_or(0) } as f64 / mf::HNS;
        // Write audio up to half a second past this video sample, so the two streams stay
        // interleaved in the file rather than all audio landing at the end.
        if let Some(pcm) = &pcm {
            let until = (((t + 0.5) * audio::RATE as f64) as usize * 2).min(pcm.len());
            if until > audio_pos {
                writer.write_audio(&pcm[audio_pos..until])?;
                audio_pos = until;
            }
        }
        writer.write_video_sample(&sample)?;
        count += 1;
        if count % 30 == 0 && duration > 0.0 {
            progress(job, (t / duration) as f32 * 0.95, "Saving recording…");
        }
    }
    // Write any audio that extends past the last video sample.
    if let Some(pcm) = &pcm {
        if audio_pos < pcm.len() {
            writer.write_audio(&pcm[audio_pos..])?;
        }
    }
    if count == 0 {
        return Err(anyhow!("the recorded video is empty"));
    }
    progress(job, 0.98, "Finishing…");
    writer.finish()
}

/// Decodes the session video and re-encodes it to H.264 with `audio` as AAC.
///
/// The fallback when [`remux`] cannot copy the stream. Odd dimensions are cropped to even ones
/// (required by NV12), the bitrate follows the quality setting in `s`, and each frame is shown
/// until the next one starts.
fn transcode(
    video: &Path,
    out: &Path,
    audio: Option<&[f32]>,
    s: &Settings,
    job: Option<&Job>,
) -> Result<()> {
    let info = mf::probe(video)?;
    let mut reader = mf::VideoReader::open(video, true)?;
    let (w, h) = ((reader.width & !1) as usize, (reader.height & !1) as usize);
    let fps = info.fps.clamp(1.0, 60.0);
    let mut writer = mf::Writer::create(
        out,
        mf::WriterOptions {
            video: Some(mf::VideoInput::Encode(mf::VideoSpec {
                width: w as u32,
                height: h as u32,
                fps,
                bitrate: crate::media::video_bitrate(w as u32, h as u32, fps, s.quality),
            })),
            audio: audio.map(|_| mf::AudioCodec::Aac),
            hardware: true,
            fragmented: false,
        },
    )?;
    let pcm = audio.map(audio::to_i16);
    let mut audio_pos = 0usize;
    let mut nv12 = vec![];
    let mut prev: Option<mf::Frame> = None;
    // Encodes one decoded frame lasting `dur` seconds, cropping it to the even output size.
    let write =
        |writer: &mut mf::Writer, f: &mf::Frame, dur: f64, nv12: &mut Vec<u8>| -> Result<()> {
            let src = if (f.width as usize, f.height as usize) == (w, h) {
                f.bgra.clone()
            } else {
                convert::crop(&f.bgra, f.width as usize, 0, 0, w, h)
            };
            convert::bgra_to_nv12(&src, w * 4, w, h, nv12);
            writer.write_nv12(nv12, f.time, dur)
        };
    // Frames are written one step behind, once the next frame's time gives their duration.
    while let Some(frame) = reader.read_frame()? {
        if let Some(p) = prev.take() {
            write(
                &mut writer,
                &p,
                (frame.time - p.time).max(1.0 / fps),
                &mut nv12,
            )?;
        }
        // Keep the audio half a second ahead of the video, as in `remux`.
        if let Some(pcm) = &pcm {
            let until = (((frame.time + 0.5) * audio::RATE as f64) as usize * 2).min(pcm.len());
            if until > audio_pos {
                writer.write_audio(&pcm[audio_pos..until])?;
                audio_pos = until;
            }
        }
        if info.duration > 0.0 {
            progress(
                job,
                (frame.time / info.duration) as f32 * 0.95,
                "Saving recording…",
            );
        }
        prev = Some(frame);
    }
    // The last frame lasts one frame interval; then flush the remaining audio.
    if let Some(p) = prev {
        write(&mut writer, &p, 1.0 / fps, &mut nv12)?;
    }
    if let Some(pcm) = &pcm {
        if audio_pos < pcm.len() {
            writer.write_audio(&pcm[audio_pos..])?;
        }
    }
    writer.finish()
}

/// Decodes the video at `video` and writes it to `out` as an animated GIF at `fps` (clamped to
/// 1..=50) frames per second.
///
/// The source is resampled to a fixed frame rate: each GIF frame shows the newest source frame
/// at its start time. Progress is reported to `job`.
pub fn write_gif(video: &Path, out: &Path, fps: f64, job: Option<&Job>) -> Result<()> {
    let info = mf::probe(video)?;
    let mut reader = mf::VideoReader::open(video, true)?;
    let (w, h) = (reader.width, reader.height);
    let mut gif = GifWriter::create(out, w, h)?;
    let step = 1.0 / fps.clamp(1.0, 50.0);
    let mut next_t = 0.0;
    let mut last: Option<mf::Frame> = None;
    while let Some(frame) = reader.read_frame()? {
        while frame.time >= next_t + step {
            // Emit the previous frame for every GIF tick that ends before this frame starts;
            // the GIF writer merges identical consecutive frames.
            if let Some(f) = &last {
                let mut rgba = f.bgra.clone();
                convert::swap_red_blue(&mut rgba);
                gif.add_rgba(rgba, step)?;
            }
            next_t += step;
        }
        if info.duration > 0.0 {
            progress(
                job,
                (frame.time / info.duration) as f32 * 0.95,
                "Creating GIF…",
            );
        }
        last = Some(frame);
    }
    // The last frame is shown for one tick.
    if let Some(f) = last {
        let mut rgba = f.bgra;
        convert::swap_red_blue(&mut rgba);
        gif.add_rgba(rgba, step)?;
    }
    gif.finish()
}

/// Encodes interleaved stereo 48 kHz `samples` (range -1..=1) to `out` as AAC (.m4a) or MP3.
pub fn write_audio_file(
    out: &Path,
    codec: mf::AudioCodec,
    samples: &[f32],
    job: Option<&Job>,
) -> Result<()> {
    progress(job, 0.1, "Encoding audio…");
    let mut writer = mf::Writer::create(
        out,
        mf::WriterOptions {
            video: None,
            audio: Some(codec),
            hardware: false,
            fragmented: false,
        },
    )?;
    writer.write_audio(&audio::to_i16(samples))?;
    writer.finish()
}

/// Composes the final file, keeps the click, mouse and marker logs next to it for the editor,
/// and on success deletes the session directory.
///
/// On failure the session directory is kept, so the recording can still be recovered later.
pub fn finalize(session: &FinishedSession, job: Option<&Job>) -> Result<PathBuf> {
    let out = compose(session, job)?;
    // Copy the session's side logs next to the video, where the editor looks for them: clicks
    // (automatic zoom), mouse positions (vertical export) and markers. Missing logs are skipped
    // and copy failures ignored, since the video itself is already complete.
    use crate::editor::project::{clicks_file, markers_file, mouse_file};
    for (name, dest) in [
        ("clicks.json", clicks_file(&out)),
        ("mouse.json", mouse_file(&out)),
        ("markers.json", markers_file(&out)),
    ] {
        let src = session.dir.join(name);
        if src.is_file() {
            let _ = std::fs::copy(&src, dest);
        }
    }
    let _ = std::fs::remove_dir_all(&session.dir);
    Ok(out)
}

/// Output path for a session recovered after a crash: `<stem> (recovered).<ext>` in the output
/// folder from `settings`, numbered ` (recovered 2)`, … if that name is taken.
///
/// `original` is the output path stored in the session's manifest.
pub fn recovered_output(settings: &Settings, original: &Path) -> PathBuf {
    let stem = original
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Recovered".into());
    let ext = original
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or("mp4".into());
    let mut p = settings
        .output_dir
        .join(format!("{stem} (recovered).{ext}"));
    let mut n = 2;
    while p.exists() {
        p = settings
            .output_dir
            .join(format!("{stem} (recovered {n}).{ext}"));
        n += 1;
    }
    p
}
