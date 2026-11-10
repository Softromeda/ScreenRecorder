// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Media capture, encoding and sample-processing building blocks.
//!
//! * [`convert`], [`audio`], [`voice`] and [`gifenc`] are pure Rust (pixel conversion, audio
//!   mixing and effects, speech detection, GIF writing) and are unit-tested on any OS.
//! * [`mf`] wraps Windows Media Foundation for H.264/AAC/MP3 encoding and for decoding.
//! * [`capture`] (DXGI Desktop Duplication) and [`wgc`] (Windows.Graphics.Capture) grab screen
//!   and window images for the recorder.
//!
//! This module also holds small helpers shared by the recorder, the editor and the library.

#[cfg(windows)]
pub mod apploop;
pub mod audio;
#[cfg(windows)]
pub mod capture;
pub mod convert;
pub mod gifenc;
#[cfg(windows)]
pub mod mf;
pub mod voice;
#[cfg(windows)]
pub mod wgc;

/// Basic stream information about a media file, as reported by [`mf::probe`].
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    /// Total length in seconds.
    pub duration: f64,
    /// Video frame width in pixels (0 when there is no video stream).
    pub width: u32,
    /// Video frame height in pixels (0 when there is no video stream).
    pub height: u32,
    /// Video frame rate in frames per second.
    pub fps: f64,
    /// Whether the file contains an audio stream.
    pub has_audio: bool,
    /// Whether the file contains a video stream.
    pub has_video: bool,
}

/// Formats a duration in seconds as `mm:ss.s`, or `h:mm:ss.s` from one hour upwards.
///
/// Negative values are shown as zero. Used for timers, the editor's time display and the
/// recordings library.
pub fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let h = (secs / 3600.0).floor() as u64;
    let m = ((secs % 3600.0) / 60.0).floor() as u64;
    let s = secs % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m:02}:{s:04.1}")
    }
}

/// Returns the H.264 bitrate in bits per second for a frame size, frame rate and the user's
/// 1..=100 quality setting.
///
/// The bits-per-pixel budget grows quadratically with quality. Screen content compresses well,
/// so high quality settings are generous; the result is clamped to 0.5–120 Mbit/s.
pub fn video_bitrate(width: u32, height: u32, fps: f64, quality: u32) -> u32 {
    let q = quality.clamp(1, 100) as f64 / 100.0;
    let bits_per_pixel = 0.03 + 0.17 * q * q;
    let bps = width as f64 * height as f64 * fps.clamp(1.0, 120.0) * bits_per_pixel;
    bps.clamp(500_000.0, 120_000_000.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Higher quality gives a higher bitrate, and full quality at 1080p60 lands in a sane range.
    #[test]
    fn bitrate_scales_with_quality() {
        let low = video_bitrate(1920, 1080, 60.0, 10);
        let high = video_bitrate(1920, 1080, 60.0, 100);
        assert!(low < high);
        assert!((20_000_000..=30_000_000).contains(&high), "{high}");
    }

    /// Durations under an hour are formatted as `mm:ss.s`.
    #[test]
    fn time_format() {
        assert_eq!(format_time(65.3), "01:05.3");
    }
}
