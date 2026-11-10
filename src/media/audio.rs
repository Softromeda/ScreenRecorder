// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Audio processing in plain Rust: WAV reading, resampling and channel conversion, mixing,
//! loudness boost, fades and time-stretching.
//!
//! The recorder writes its microphone and speaker tracks as WAV files; when a recording is saved
//! or the editor exports, they are read with [`read_wav`], converted to the common format with
//! [`Converter`] / [`to_stereo_48k`], mixed with [`mix`] and, for the editor's speed control,
//! stretched with [`Stretcher`]. All processing after conversion uses interleaved stereo `f32`
//! samples in -1..=1 at [`RATE`] Hz.

use std::path::Path;

use anyhow::{Context, Result};

/// Sample rate in Hz of all processed and encoded audio.
pub const RATE: u32 = 48_000;

/// Interleaved audio in its source sample rate and channel count, as `f32` samples in -1..=1.
#[derive(Debug, Clone, Default)]
pub struct Pcm {
    /// Sample rate in Hz.
    pub rate: u32,
    /// Number of interleaved channels (at least 1).
    pub channels: usize,
    /// Interleaved samples, `channels` per frame.
    pub samples: Vec<f32>,
}

impl Pcm {
    /// Number of sample frames (one sample per channel each).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Length in seconds.
    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.rate.max(1) as f64
    }
}

/// Reads a whole WAV file into memory as `f32` samples.
///
/// Accepts float and integer PCM of any bit depth (covering every format the recorder writes);
/// integer samples are scaled to -1..=1. Reading stops quietly at the first corrupt or truncated
/// sample, so a recording cut short by a crash still yields its intact part.
pub fn read_wav(path: &Path) -> Result<Pcm> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map_while(|s| s.ok()).collect(),
        hound::SampleFormat::Int => {
            // Full scale for N-bit signed samples is 2^(N-1).
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map_while(|s| s.ok())
                .map(|s| s as f32 * scale)
                .collect()
        }
    };
    Ok(Pcm {
        rate: spec.sample_rate,
        channels: spec.channels.max(1) as usize,
        samples,
    })
}

/// Streaming converter from any sample rate and channel count to stereo at [`RATE`].
///
/// Resamples by linear interpolation and keeps its position and the last input frame between
/// calls, so feeding a stream in chunks gives the same result as converting it in one piece.
pub struct Converter {
    /// Input sample rate in Hz (at least 1).
    in_rate: u32,
    /// Number of interleaved input channels (at least 1).
    in_channels: usize,
    /// Position of the next output frame, in input frames relative to the start of the next
    /// chunk; -1 refers to the last frame of the previous chunk.
    pos: f64,
    /// Last input frame of the previous chunk (for interpolation across chunks).
    prev: [f32; 2],
    /// Whether `prev` holds a real frame (false until the first chunk has been processed).
    has_prev: bool,
}

impl Converter {
    /// Creates a converter for input at `in_rate` Hz with `in_channels` interleaved channels.
    pub fn new(in_rate: u32, in_channels: usize) -> Self {
        Self {
            in_rate: in_rate.max(1),
            in_channels: in_channels.max(1),
            pos: 0.0,
            prev: [0.0; 2],
            has_prev: false,
        }
    }

    /// Returns frame `frame` of `chunk` as a stereo pair.
    fn stereo(&self, chunk: &[f32], frame: usize) -> [f32; 2] {
        stereo_frame(chunk, self.in_channels, frame)
    }

    /// Converts one chunk of interleaved input and appends the stereo [`RATE`] output to `out`.
    ///
    /// A trailing partial frame in `chunk` is ignored.
    pub fn push(&mut self, chunk: &[f32], out: &mut Vec<f32>) {
        let frames = chunk.len() / self.in_channels;
        if frames == 0 {
            return;
        }
        let step = self.in_rate as f64 / RATE as f64;
        let prev = self.prev;
        let channels = self.in_channels;
        // Input frame index -1 refers to the last frame of the previous chunk.
        let get = |i: i64| -> [f32; 2] {
            if i < 0 {
                prev
            } else {
                stereo_frame(chunk, channels, (i as usize).min(frames - 1))
            }
        };
        // Emit every output frame whose position falls within this chunk, interpolating between
        // the two neighbouring input frames.
        let mut pos = self.pos.max(if self.has_prev { -1.0 } else { 0.0 });
        while pos <= (frames - 1) as f64 {
            let i = pos.floor() as i64;
            let frac = (pos - i as f64) as f32;
            let a = get(i);
            let b = get(i + 1);
            out.push(a[0] + (b[0] - a[0]) * frac);
            out.push(a[1] + (b[1] - a[1]) * frac);
            pos += step;
        }
        // Carry the position (rebased to the next chunk) and the last frame over.
        self.pos = pos;
        self.prev = self.stereo(chunk, frames - 1);
        self.has_prev = true;
        self.pos -= frames as f64;
    }
}

/// Returns frame `frame` of an interleaved buffer with `channels` channels as a stereo pair.
///
/// Mono is duplicated to both sides; with three or more channels, front left/right are taken
/// and the centre channel (index 2) is mixed into both at about -3 dB.
fn stereo_frame(chunk: &[f32], channels: usize, frame: usize) -> [f32; 2] {
    let s = &chunk[frame * channels..frame * channels + channels];
    match channels {
        1 => [s[0], s[0]],
        2 => [s[0], s[1]],
        // Down-mix surround: front left/right plus centre; other channels are dropped.
        _ => [s[0] + s[2] * 0.7, s[1] + s[2] * 0.7],
    }
}

/// Converts a whole buffer to interleaved stereo at [`RATE`] (a plain copy when it already is).
pub fn to_stereo_48k(pcm: &Pcm) -> Vec<f32> {
    if pcm.rate == RATE && pcm.channels == 2 {
        return pcm.samples.clone();
    }
    let mut out = Vec::with_capacity((pcm.duration() * RATE as f64) as usize * 2 + 16);
    Converter::new(pcm.rate, pcm.channels).push(&pcm.samples, &mut out);
    out
}

/// Raises a quiet recording (e.g. system audio) towards a comfortable level in place.
///
/// The gain brings the peak to about -1 dB (0.89), is capped at 4× and never attenuates. Near
/// silent input and gains below 1% are left alone.
pub fn auto_boost(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak < 1e-4 {
        return;
    }
    let gain = (0.89 / peak).clamp(1.0, 4.0);
    if gain > 1.01 {
        for s in samples.iter_mut() {
            *s *= gain;
        }
    }
}

/// Returns a short mouse-click sound: mono 48 kHz samples, about 30 ms long.
///
/// A sharp, quickly decaying burst of noise (the "tick") on top of a brief 2.2 kHz tone and a
/// softer 180 Hz body, so it sounds like a mouse button rather than a beep. Peaks stay below 0.5.
pub fn click_sound() -> Vec<f32> {
    let n = (RATE as f64 * 0.030) as usize;
    let mut seed: u32 = 0x1234_5678; // fixed seed: every click sounds the same
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            // Numerical Recipes LCG; the top 24 bits are mapped to white noise in -1..1.
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            let tau = std::f32::consts::TAU;
            let tick = noise * (-t / 0.0012).exp();
            let tone = (tau * 2200.0 * t).sin() * (-t / 0.004).exp();
            let body = (tau * 180.0 * t).sin() * (-t / 0.008).exp();
            // 1 ms fade-in avoids a pop at the very first sample.
            let attack = (t / 0.001).min(1.0);
            attack * (0.30 * tick + 0.12 * tone + 0.06 * body)
        })
        .collect()
}

/// Returns an interleaved stereo 48 kHz track, `frames` frames long, with [`click_sound`] at
/// each time in `times` (seconds). Clicks past the end are dropped or cut short.
pub fn click_track(times: &[f64], frames: usize) -> Vec<f32> {
    let click = click_sound();
    let mut out = vec![0.0f32; frames * 2];
    for &t in times {
        let start = (t.max(0.0) * RATE as f64) as usize;
        for (i, s) in click.iter().enumerate() {
            let f = start + i;
            if f >= frames {
                break;
            }
            out[f * 2] += s;
            out[f * 2 + 1] += s;
        }
    }
    out
}

/// Mixes several interleaved stereo tracks by summing them.
///
/// The result is as long as the longest track. When more than one track is mixed, the sum is
/// passed through a soft limiter so peaks above 0.9 do not clip harshly.
pub fn mix(tracks: &[Vec<f32>]) -> Vec<f32> {
    let len = tracks.iter().map(|t| t.len()).max().unwrap_or(0);
    let mut out = vec![0.0f32; len];
    for t in tracks {
        for (o, s) in out.iter_mut().zip(t) {
            *o += *s;
        }
    }
    if tracks.len() > 1 {
        for s in out.iter_mut() {
            *s = soft_clip(*s);
        }
    }
    out
}

/// Soft limiter: linear up to ±0.9, then compressed smoothly towards ±1.0 with `tanh`.
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) / 0.1).tanh())
    }
}

/// Converts `f32` samples to 16-bit PCM, clamping to -1..=1 first.
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Streaming overlap-add (OLA) time stretcher for interleaved stereo at [`RATE`].
///
/// Hann-windowed 40 ms grains are read from the input every `hop_out * speed` frames and
/// written to the output every `hop_out` frames, so a `speed` above 1 makes the audio shorter and
/// below 1 makes it longer while the pitch stays the same. Used for the editor's speed control.
pub struct Stretcher {
    /// Playback speed, clamped to 0.25..=4.
    speed: f64,
    /// Grain length in frames (40 ms).
    window: usize,
    /// Output hop in frames (half a window, i.e. 50% overlap).
    hop_out: usize,
    /// Buffered interleaved input not yet consumed.
    input: Vec<f32>,
    /// Read position in `input` (frames, fractional).
    read: f64,
    /// Overlap-add accumulator for the next `window` output frames (interleaved stereo).
    acc: Vec<f32>,
    /// Sum of window weights per accumulated frame, used to normalise the output.
    norm: Vec<f32>,
    /// Hann window of `window` coefficients.
    hann: Vec<f32>,
}

impl Stretcher {
    /// Creates a stretcher for the given playback speed (clamped to 0.25..=4).
    pub fn new(speed: f64) -> Self {
        let window = (RATE as usize * 40) / 1000; // 40 ms
        let hann = (0..window)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / window as f32).cos())
            .collect();
        Self {
            speed: speed.clamp(0.25, 4.0),
            window,
            hop_out: window / 2,
            input: vec![],
            read: 0.0,
            acc: vec![0.0; window * 2],
            norm: vec![0.0; window],
            hann,
        }
    }

    /// Whether the speed is 1, in which case input is passed through unchanged.
    pub fn is_identity(&self) -> bool {
        (self.speed - 1.0).abs() < 1e-3
    }

    /// Feeds interleaved stereo input and appends any stretched output that is complete to `out`.
    ///
    /// Input is buffered until a whole window is available, so output lags the input by up to
    /// one window; the remainder at the end of a stream is not flushed.
    pub fn push(&mut self, chunk: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(chunk);
            return;
        }
        self.input.extend_from_slice(chunk);
        let hop_in = self.hop_out as f64 * self.speed;
        let w = self.window;
        loop {
            let start = self.read as usize;
            if (start + w) * 2 > self.input.len() {
                break;
            }
            // Overlap-add this window into the accumulator.
            for i in 0..w {
                let g = self.hann[i];
                self.acc[i * 2] += self.input[(start + i) * 2] * g;
                self.acc[i * 2 + 1] += self.input[(start + i) * 2 + 1] * g;
                self.norm[i] += g;
            }
            // No later window overlaps the first hop of the accumulator any more: normalise it
            // by the summed window weights, emit it and shift the accumulator along.
            for i in 0..self.hop_out {
                let n = self.norm[i].max(1e-3);
                out.push(self.acc[i * 2] / n);
                out.push(self.acc[i * 2 + 1] / n);
            }
            self.acc.drain(0..self.hop_out * 2);
            self.acc.resize(w * 2, 0.0);
            self.norm.drain(0..self.hop_out);
            self.norm.resize(w, 0.0);
            self.read += hop_in;
        }
        // Drop whole input frames that no future window will read.
        let consumed = self.read as usize;
        if consumed > 0 {
            self.input.drain(0..consumed * 2);
            self.read -= consumed as f64;
        }
    }
}

/// Returns the linear fade gain (0..=1) at time `t` of a clip `total` seconds long, with fade-in
/// and fade-out lengths in seconds (0 disables either fade).
pub fn fade_gain(t: f64, total: f64, fade_in: f64, fade_out: f64) -> f32 {
    let mut g = 1.0;
    if fade_in > 0.0 && t < fade_in {
        g *= t / fade_in;
    }
    if fade_out > 0.0 && t > total - fade_out {
        g *= ((total - t) / fade_out).max(0.0);
    }
    g.clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clicks land at their times in both channels, stay below clipping, and a click past the
    /// end is dropped.
    #[test]
    fn click_track_places_clicks() {
        let frames = RATE as usize; // 1 second
        let t = click_track(&[0.25, 0.75, 5.0], frames);
        assert_eq!(t.len(), frames * 2);
        let peak = t.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05 && peak < 0.5, "peak {peak}");
        let energy = |from: f64, to: f64| -> f32 {
            let (a, b) = ((from * RATE as f64) as usize, (to * RATE as f64) as usize);
            t[a * 2..b * 2].iter().map(|s| s * s).sum()
        };
        assert!(energy(0.25, 0.30) > 0.01);
        assert!(energy(0.75, 0.80) > 0.01);
        assert_eq!(energy(0.0, 0.24), 0.0);
        assert_eq!(energy(0.40, 0.70), 0.0);
        // Left and right are identical.
        assert!(t.chunks(2).all(|f| f[0] == f[1]));
    }

    /// Mono 24 kHz becomes stereo 48 kHz with twice the frames and unchanged level.
    #[test]
    fn converter_resamples_and_upmixes() {
        let pcm = Pcm {
            rate: 24_000,
            channels: 1,
            samples: vec![0.5; 24_000],
        };
        let out = to_stereo_48k(&pcm);
        assert!((out.len() as i64 - 96_000).abs() < 8, "{}", out.len());
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }

    /// Converting in chunks matches converting in one piece (no gaps or clicks at chunk joins).
    #[test]
    fn converter_streams_without_gaps() {
        let tone: Vec<f32> = (0..44_100).map(|i| ((i as f32) * 0.01).sin()).collect();
        let mut one = vec![];
        Converter::new(44_100, 1).push(&tone, &mut one);
        let mut chunked = vec![];
        let mut c = Converter::new(44_100, 1);
        for part in tone.chunks(1000) {
            c.push(part, &mut chunked);
        }
        assert!((one.len() as i64 - chunked.len() as i64).abs() <= 4);
        let n = one.len().min(chunked.len());
        let diff = one[..n]
            .iter()
            .zip(&chunked[..n])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(diff < 0.02, "max diff {diff}");
    }

    /// Mixing takes the longest length and soft-limits the sum; auto boost brings the peak to 0.8
    /// here (the 4× cap).
    #[test]
    fn mixing_and_boost() {
        let a = vec![0.5f32; 10];
        let b = vec![0.6f32; 8];
        let m = mix(&[a, b]);
        assert_eq!(m.len(), 10);
        assert!(m[0] <= 1.0 && m[0] > 0.9);
        let mut quiet = vec![0.1f32, -0.2];
        auto_boost(&mut quiet);
        assert!((quiet[1] + 0.8).abs() < 1e-6);
    }

    /// Stretching at 2× and 0.5× changes the length by the expected factor without overshooting.
    #[test]
    fn stretcher_changes_length() {
        let input: Vec<f32> = (0..RATE as usize * 2 * 2)
            .map(|i| ((i / 2) as f32 * 0.05).sin())
            .collect();
        for speed in [2.0, 0.5] {
            let mut s = Stretcher::new(speed);
            let mut out = vec![];
            for part in input.chunks(4800) {
                s.push(part, &mut out);
            }
            let expected = input.len() as f64 / speed;
            let ratio = out.len() as f64 / expected;
            assert!((0.9..1.05).contains(&ratio), "speed {speed}: ratio {ratio}");
            assert!(out.iter().all(|v| v.abs() <= 1.01));
        }
    }

    /// Fade gain is 0 at the start, 1 in the middle and halfway through the fade-out at 0.5.
    #[test]
    fn fades() {
        assert_eq!(fade_gain(0.0, 10.0, 1.0, 1.0), 0.0);
        assert_eq!(fade_gain(5.0, 10.0, 1.0, 1.0), 1.0);
        assert!((fade_gain(9.5, 10.0, 1.0, 1.0) - 0.5).abs() < 1e-6);
    }
}
