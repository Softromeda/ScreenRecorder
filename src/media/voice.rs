// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Voice processing: microphone clean-up, pause detection and background music mixing.
//!
//! * [`denoise`] removes steady background noise (fans, hum, hiss) by spectral gating and
//!   [`level`] keeps speech at an even, comfortable level without clipping; the recorder applies
//!   both to the microphone track when a recording is saved, if enabled in the options.
//! * [`find_pauses`] finds quiet stretches for the editor's "Remove pauses" command.
//! * [`MusicMixer`] mixes looped background music under an export, ducking it under speech.
//!
//! Everything works on interleaved stereo `f32` at [`RATE`]. A microphone is mono, so the
//! clean-up functions process the averaged channels and write the result back to both.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::audio::RATE;

/// FFT size in samples (21 ms at 48 kHz).
const N: usize = 1024;
/// Hop between analysis frames: 50 % overlap.
const HOP: usize = N / 2;
/// Number of non-negative frequency bins of a real N-point FFT.
const BINS: usize = N / 2 + 1;

/// Averages interleaved stereo into mono.
fn to_mono(stereo: &[f32]) -> Vec<f32> {
    stereo
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| (s[0] + s[1]) * 0.5)
        .collect()
}

/// Writes a mono signal to both channels of an interleaved stereo buffer of the same length.
fn to_stereo(mono: &[f32], stereo: &mut [f32]) {
    for (s, m) in stereo.as_chunks_mut::<2>().0.iter_mut().zip(mono) {
        s[0] = *m;
        s[1] = *m;
    }
}

/// Returns the square-root Hann window of length [`N`].
///
/// Applied at both analysis and synthesis, its square is a Hann window, whose copies sum to one at
/// 50 % overlap, so unmodified spectra reconstruct the signal exactly.
fn window() -> Vec<f32> {
    (0..N)
        .map(|i| {
            let h = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos();
            h.sqrt()
        })
        .collect()
}

/// Forward and inverse FFT plans of size [`N`] with the shared analysis/synthesis window.
struct Stft {
    /// Forward FFT plan of size [`N`].
    fwd: Arc<dyn Fft<f32>>,
    /// Inverse FFT plan of size [`N`] (unnormalised: output is scaled by `N`).
    inv: Arc<dyn Fft<f32>>,
    /// Square-root Hann window from [`window`], used for analysis and synthesis.
    win: Vec<f32>,
}

impl Stft {
    /// Plans the forward and inverse FFTs.
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            fwd: planner.plan_fft_forward(N),
            inv: planner.plan_fft_inverse(N),
            win: window(),
        }
    }

    /// Fills `buf` with the spectrum of the windowed frame of `x` starting at sample `start`;
    /// samples past the end of `x` count as zero.
    fn spectrum(&self, x: &[f32], start: usize, buf: &mut [Complex32]) {
        for (i, b) in buf.iter_mut().enumerate() {
            let s = x.get(start + i).copied().unwrap_or(0.0);
            *b = Complex32::new(s * self.win[i], 0.0);
        }
        self.fwd.process(buf);
    }
}

/// Removes steady background noise from a microphone track (interleaved stereo) in place.
///
/// Estimates the noise spectrum from the quietest moments of the whole recording, then
/// attenuates each frequency of each frame by how far it stands above that noise.
///
/// `over` is the over-subtraction factor: values above 1 remove noise more firmly than its
/// estimated average power, so noise peaks above the average are gated too. `max_reduction_db`
/// caps the attenuation of frequencies that contain only noise. Tracks shorter than four FFT
/// frames and digital silence are left unchanged.
pub fn denoise(stereo: &mut [f32], over: f32, max_reduction_db: f32) {
    let x = to_mono(stereo);
    if x.len() < N * 4 {
        return;
    }
    let stft = Stft::new();
    let frames = x.len().div_ceil(HOP) + 1;
    let mut buf = vec![Complex32::default(); N];

    // 1. Noise profile: for each frequency, a low percentile of its power over the recording
    //    (the level it has in the pauses). At most about 3000 frames are sampled to bound the
    //    cost on long recordings.
    let step = (frames / 3000).max(1);
    let mut samples: Vec<Vec<f32>> = (0..BINS)
        .map(|_| Vec::with_capacity(frames / step + 1))
        .collect();
    for f in (0..frames).step_by(step) {
        stft.spectrum(&x, f * HOP, &mut buf);
        for (k, s) in samples.iter_mut().enumerate() {
            s.push(buf[k].norm_sqr());
        }
    }
    // Noise power per frequency varies a lot from frame to frame (exponentially distributed):
    // its 15th percentile is about 0.16 × its average, so scale it up to estimate the average.
    let noise: Vec<f32> = samples
        .into_iter()
        .map(|mut v| {
            v.sort_by(|a, b| a.total_cmp(b));
            v[(v.len() as f32 * 0.15) as usize] / 0.1625
        })
        .collect();
    if noise.iter().all(|n| *n <= 1e-12) {
        return; // digital silence: nothing to remove
    }

    // 2. Spectral gating: a Wiener-like gain per bin from the noise-to-signal ratio, limited
    //    to the maximum reduction and smoothed over time and frequency (against "musical
    //    noise"), then overlap-added back into the time domain.
    let floor = 10f32.powf(-max_reduction_db / 20.0);
    let mut out = vec![0.0f32; x.len() + N];
    let mut prev = vec![1.0f32; BINS];
    let mut gain = vec![1.0f32; BINS];
    for f in 0..frames {
        let start = f * HOP;
        stft.spectrum(&x, start, &mut buf);
        for k in 0..BINS {
            let p = buf[k].norm_sqr().max(1e-20);
            // Power-domain gain (1 - noise/signal), floored at the maximum reduction; the
            // square root turns it into an amplitude gain for the complex bin.
            let g = (1.0 - over * noise[k] / p).max(floor * floor).sqrt();
            // Open immediately for speech, close gradually.
            gain[k] = if g > prev[k] {
                g
            } else {
                prev[k] * 0.5 + g * 0.5
            };
        }
        prev.copy_from_slice(&gain);
        for k in 0..BINS {
            // Smooth across neighbouring frequencies too.
            let a = gain[k.saturating_sub(1)];
            let b = gain[(k + 1).min(BINS - 1)];
            let g = (a + 2.0 * gain[k] + b) / 4.0;
            buf[k] *= g;
            if k > 0 && k < N / 2 {
                buf[N - k] *= g; // keep the spectrum symmetric (real signal)
            }
        }
        stft.inv.process(&mut buf);
        // Synthesis window and overlap-add; the inverse FFT is unnormalised, hence `/ N`.
        for (i, (b, w)) in buf.iter().zip(&stft.win).enumerate() {
            if let Some(o) = out.get_mut(start + i) {
                *o += b.re * w / N as f32;
            }
        }
    }
    out.truncate(x.len());
    to_stereo(&out, stereo);
}

/// Brings speech in a microphone track (interleaved stereo) to an even level in place and stops
/// peaks from clipping.
///
/// Works on 10 ms blocks: speech blocks are those well above the noise floor; a slow automatic
/// gain (0.5×–6×) moves their 400 ms loudness towards -20 dBFS RMS, a look-around peak limiter
/// keeps peaks below 0.95, and the per-block gains are interpolated sample by sample.
pub fn level(stereo: &mut [f32]) {
    let mut x = to_mono(stereo);
    if x.is_empty() {
        return;
    }
    // RMS level of each 10 ms block.
    let block = (RATE / 100) as usize; // 10 ms
    let blocks = x.len().div_ceil(block);
    let rms: Vec<f32> = (0..blocks)
        .map(|b| {
            let s = &x[b * block..((b + 1) * block).min(x.len())];
            (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
        })
        .collect();

    // Speech: clearly above the quietest parts (the 10th percentile is taken as the noise
    // floor).
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[sorted.len() / 10].max(1e-5);
    let threshold = (floor * 4.0).max(0.003); // and above -50 dBFS

    const TARGET: f32 = 0.1; // speech level: -20 dBFS RMS
    const MAX_GAIN: f32 = 6.0; // +15.5 dB
    const MIN_GAIN: f32 = 0.5; // -6 dB
    // Automatic gain control: follow the speech loudness over a sliding window, ignoring
    // pauses so that the gain does not pump up the background noise.
    let win = 40; // 400 ms loudness window
    let mut gains = vec![1.0f32; blocks];
    let mut g = 1.0f32;
    let mut energy = 0.0f32;
    let mut count = 0usize;
    let mut silent_for = 0usize;
    for b in 0..blocks {
        // Running mean energy of the speech blocks in the window (add the new block, drop the
        // one leaving the window).
        if rms[b] > threshold {
            energy += rms[b] * rms[b];
            count += 1;
        }
        if b >= win && rms[b - win] > threshold {
            energy -= rms[b - win] * rms[b - win];
            count -= 1;
        }
        if rms[b] > threshold && count > 0 {
            silent_for = 0;
            let loud = (energy.max(0.0) / count as f32).sqrt().max(1e-6);
            let want = (TARGET / loud).clamp(MIN_GAIN, MAX_GAIN);
            // Turn down quickly, up slowly.
            g += (want - g) * if want < g { 0.3 } else { 0.03 };
        } else {
            // Pauses keep the gain; long silence drifts back to 1×.
            silent_for += 1;
            if silent_for > 100 {
                g += (1.0 - g) * 0.01;
            }
        }
        gains[b] = g;
    }

    // Peak limiter. Gains are blended between neighbouring blocks, so each block's limit
    // allows for the largest gain next to it, and each block takes the strictest limit around
    // it. The limit drops instantly and recovers slowly.
    let peaks: Vec<f32> = (0..blocks)
        .map(|b| {
            x[b * block..((b + 1) * block).min(x.len())]
                .iter()
                .fold(0.0f32, |m, v| m.max(v.abs()))
        })
        .collect();
    let near = |b: usize| b.saturating_sub(1)..=(b + 1).min(blocks - 1);
    let mut lim = vec![1.0f32; blocks];
    let mut l = 1.0f32;
    for b in 0..blocks {
        let g = near(b).map(|i| gains[i]).fold(0.0f32, f32::max);
        let peak = peaks[b] * g;
        let want = if peak > 0.95 { 0.95 / peak } else { 1.0 };
        l = if want < l {
            want
        } else {
            l + (want - l) * 0.05
        };
        lim[b] = l;
    }
    let gains: Vec<f32> = (0..blocks)
        .map(|b| gains[b] * near(b).map(|i| lim[i]).fold(1.0f32, f32::min))
        .collect();

    // Apply, interpolating between block centres for smooth changes.
    for (i, v) in x.iter_mut().enumerate() {
        let pos = i as f32 / block as f32 - 0.5;
        let b0 = (pos.floor().max(0.0) as usize).min(blocks - 1);
        let b1 = (b0 + 1).min(blocks - 1);
        let t = (pos - b0 as f32).clamp(0.0, 1.0);
        let g = gains[b0] + (gains[b1] - gains[b0]) * t;
        *v = (*v * g).clamp(-1.0, 1.0);
    }
    to_stereo(&x, stereo);
}

/// Finds pauses in speech: quiet stretches of at least `min_pause` seconds.
///
/// Returns (start, end) spans in seconds, ready to be cut by the editor. Each span leaves `keep`
/// seconds of the quiet next to the speech, so cuts do not clip words or sound abrupt; silence
/// at the very start or end of the track gets no margin on that side. The quiet threshold adapts
/// to the recording's noise floor and speech level, measured over 20 ms blocks. Returns nothing
/// for tracks shorter than 0.2 s.
pub fn find_pauses(stereo: &[f32], min_pause: f64, keep: f64) -> Vec<(f64, f64)> {
    let x = to_mono(stereo);
    let block = (RATE / 50) as usize; // 20 ms
    let blocks = x.len() / block;
    if blocks < 10 {
        return vec![];
    }
    let rms: Vec<f32> = (0..blocks)
        .map(|b| {
            let s = &x[b * block..(b + 1) * block];
            (s.iter().map(|v| v * v).sum::<f32>() / block as f32).sqrt()
        })
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[blocks / 10];
    let loud = sorted[blocks * 9 / 10];
    // Quiet: near the noise floor, and well below the speech level.
    let threshold = (floor * 3.0).max(loud * 0.05).max(0.001);

    // Scan for runs of quiet blocks; the extra iteration at `blocks` closes a run that lasts
    // until the end.
    let secs = |b: usize| b as f64 * block as f64 / RATE as f64;
    let mut out = vec![];
    let mut start: Option<usize> = None;
    for b in 0..=blocks {
        let quiet = rms.get(b).is_some_and(|r| *r < threshold);
        match (quiet, start) {
            (true, None) => start = Some(b),
            (false, Some(s)) => {
                let (a, e) = (secs(s), secs(b));
                // Silence at the very start or end needs no margin there.
                let lead = if s == 0 { 0.0 } else { keep };
                let tail = if b == blocks { 0.0 } else { keep };
                if e - a >= min_pause && e - a > lead + tail + 0.05 {
                    out.push((a + lead, e - tail));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// Background music mixed under a video's sound during export.
///
/// The music is looped to fill the video, faded in over 1 s and out over the last 3 s, and
/// optionally lowered ("ducked") while someone speaks. It keeps the ducking state between calls,
/// so the output can be mixed in chunks of any size.
pub struct MusicMixer {
    /// Interleaved stereo music samples at [`RATE`].
    samples: Vec<f32>,
    /// Music volume multiplier.
    volume: f32,
    /// Whether to lower the music while speech is detected.
    duck: bool,
    /// Current ducking gain (1 = full level).
    env: f32,
    /// Number of 10 ms blocks since speech was last detected.
    quiet_blocks: usize,
}

impl MusicMixer {
    /// Creates a mixer for `samples` (interleaved stereo at [`RATE`]) at the given volume.
    pub fn new(samples: Vec<f32>, volume: f32, duck: bool) -> Self {
        Self {
            samples,
            volume,
            duck,
            env: 1.0,
            // Start as if speech had been absent for a long time (music at full level).
            quiet_blocks: usize::MAX / 2,
        }
    }

    /// Adds music to `buf` (interleaved stereo) in place, clamping to -1..=1.
    ///
    /// The first frame of `buf` is output frame `start` of `total` frames; these positions drive
    /// the looping and the fades. Speech is detected from the sound already in `buf`.
    pub fn mix_into(&mut self, buf: &mut [f32], start: usize, total: usize) {
        let frames = self.samples.len() / 2;
        if frames == 0 {
            return;
        }
        const DUCKED: f32 = 0.3; // music level while speaking (about -10 dB)
        const SPEECH: f32 = 0.015; // speech detection level (about -36 dBFS)
        const HOLD: usize = 30; // keep lowered 0.3 s after speech (between words)
        let block = (RATE / 100) as usize;
        let fade_in = RATE as f32; // 1 s
        let fade_out = RATE as f32 * 3.0; // 3 s
        for (b, chunk) in buf.chunks_mut(block * 2).enumerate() {
            // Ducking: detect speech in this 10 ms block and move the music gain towards the
            // lowered level, holding it briefly between words.
            if self.duck {
                let rms =
                    (chunk.iter().map(|v| v * v).sum::<f32>() / chunk.len().max(1) as f32).sqrt();
                if rms > SPEECH {
                    self.quiet_blocks = 0;
                } else {
                    self.quiet_blocks += 1;
                }
                let target = if self.quiet_blocks <= HOLD {
                    DUCKED
                } else {
                    1.0
                };
                // Down quickly (30 ms), back up slowly (0.4 s).
                self.env += (target - self.env) * if target < self.env { 0.3 } else { 0.025 };
            }
            // Add the looped music with the fade and ducking gains.
            for (i, s) in chunk.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let n = start + b * block + i;
                let fade = (n as f32 / fade_in)
                    .min(total.saturating_sub(n) as f32 / fade_out)
                    .clamp(0.0, 1.0);
                let g = self.volume * self.env * fade;
                let m = (n % frames) * 2;
                s[0] = (s[0] + self.samples[m] * g).clamp(-1.0, 1.0);
                s[1] = (s[1] + self.samples[m + 1] * g).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white noise.
    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut seed = 12345u32;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Duplicates a mono signal into interleaved stereo.
    fn stereo(mono: &[f32]) -> Vec<f32> {
        mono.iter().flat_map(|v| [*v, *v]).collect()
    }

    /// Root-mean-square level of a signal.
    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Noise reduction lowers noise-only parts by at least 10 dB and keeps a tone within 1.5 dB.
    #[test]
    fn denoise_removes_noise_but_keeps_the_voice() {
        // 4 s: noise everywhere, a 440 Hz "voice" in the middle two seconds.
        let n = RATE as usize * 4;
        let mut x = noise(n, 0.02);
        let (a, b) = (RATE as usize, RATE as usize * 3);
        for (i, v) in x.iter_mut().enumerate().take(b).skip(a) {
            *v += 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin();
        }
        let before_noise = rms(&x[..a / 2]);
        let before_voice = rms(&x[a + 4800..b - 4800]);
        let mut s = stereo(&x);
        denoise(&mut s, 3.0, 15.0);
        let y = to_mono(&s);
        let after_noise = rms(&y[..a / 2]);
        let after_voice = rms(&y[a + 4800..b - 4800]);
        // Noise at least 10 dB quieter, voice within 1.5 dB.
        assert!(
            after_noise < before_noise * 0.32,
            "{after_noise} vs {before_noise}"
        );
        let ratio = after_voice / before_voice;
        assert!((0.84..1.19).contains(&ratio), "voice changed by {ratio}");
    }

    /// Levelling raises quiet speech and keeps a following loud burst below clipping.
    #[test]
    fn level_raises_quiet_speech_and_never_clips() {
        // Quiet "speech" (bursts at -35 dBFS), then a loud burst near 0 dBFS.
        let sr = RATE as usize;
        let mut x = vec![0.0f32; sr * 8];
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            let on = (t * 2.0).fract() < 0.7; // syllable-like on/off
            let amp = if t < 6.0 { 0.025 } else { 0.98 };
            if on {
                *v = amp * (2.0 * std::f32::consts::PI * 220.0 * t).sin();
            }
        }
        let mut s = stereo(&x);
        level(&mut s);
        let y = to_mono(&s);
        let quiet = rms(&y[sr * 4..sr * 5]);
        assert!(quiet > 0.05, "quiet speech should be raised, got {quiet}");
        let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= 0.97, "peak {peak}");
    }

    /// Long pauses are found with margins next to speech; short ones are ignored and the final
    /// pause runs to the end.
    #[test]
    fn pauses_are_found_with_margins() {
        // 1 s speech, 2 s pause, 1 s speech, 0.3 s pause, 1 s speech, 1 s pause.
        let sr = RATE as usize;
        let mut x = noise(sr * 7, 0.002); // a little room noise
        let speech = [(0.0, 1.0), (3.0, 4.0), (4.3, 5.3)];
        for (a, b) in speech {
            for (i, v) in x
                .iter_mut()
                .enumerate()
                .take((b * sr as f64) as usize)
                .skip((a * sr as f64) as usize)
            {
                *v += 0.2 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / sr as f32).sin();
            }
        }
        let p = find_pauses(&stereo(&x), 0.8, 0.15);
        assert_eq!(p.len(), 2, "{p:?}");
        // The 2 s pause, with 0.15 s kept next to the speech on each side.
        assert!(
            (p[0].0 - 1.15).abs() < 0.03 && (p[0].1 - 2.85).abs() < 0.03,
            "{p:?}"
        );
        // The 0.3 s pause is too short; the final pause has no margin at the end.
        assert!(
            (p[1].0 - 5.45).abs() < 0.03 && (p[1].1 - 7.0).abs() < 0.03,
            "{p:?}"
        );
    }

    /// Music fades in, loops at full level, ducks under speech, recovers and fades out, when
    /// mixed in uneven chunks.
    #[test]
    fn music_loops_fades_and_ducks_under_speech() {
        let sr = RATE as usize;
        // 1 s of constant stereo "music" at 0.5, looped under 14 s of video.
        let music = vec![0.5f32; sr * 2];
        let mut m = MusicMixer::new(music, 1.0, true);
        let total = sr * 14;
        let mut out = vec![0.0f32; total * 2];
        // Speech from 4 s to 6 s.
        for (i, v) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            if (4.0..6.0).contains(&t) {
                let s = 0.2 * (2.0 * std::f32::consts::PI * 200.0 * t).sin();
                v[0] = s;
                v[1] = s;
            }
        }
        let speech: Vec<f32> = out.clone();
        // Mix in uneven chunks, like the renderer does.
        let mut pos = 0;
        for size in [1000usize, 4800, 77, 30000].iter().cycle() {
            if pos >= total {
                break;
            }
            let n = (*size).min(total - pos);
            m.mix_into(&mut out[pos * 2..(pos + n) * 2], pos, total);
            pos += n;
        }
        let music_at = |t: f32| {
            let i = (t * sr as f32) as usize * 2;
            out[i] - speech[i]
        };
        assert!(music_at(0.0).abs() < 0.01, "fades in");
        assert!((music_at(2.5) - 0.5).abs() < 0.01, "full level, looped");
        assert!(
            (music_at(5.0) - 0.15).abs() < 0.02,
            "lowered while speaking"
        );
        assert!((music_at(9.0) - 0.5).abs() < 0.02, "back up after speech");
        assert!(
            (music_at(12.5) - 0.25).abs() < 0.02,
            "fading out at the end"
        );
        assert!(music_at(13.999).abs() < 0.01, "silent at the very end");
    }

    /// Noise reduction and levelling leave digital silence exactly silent.
    #[test]
    fn silence_stays_silent() {
        let mut s = vec![0.0f32; RATE as usize * 2];
        denoise(&mut s, 3.0, 15.0);
        level(&mut s);
        assert!(s.iter().all(|v| *v == 0.0));
    }
}
