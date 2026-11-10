// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Animated GIF writer used when a recording or an editor export is saved as GIF.
//!
//! [`GifWriter`] takes RGBA frames with their display durations, quantises each to a 256-colour
//! palette with NeuQuant (via the `gif` crate), merges consecutive identical frames into one
//! longer frame and keeps the total playing time exact despite GIF's 1/100 s delay resolution.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};

/// Streaming writer for a looping animated GIF file.
///
/// Each frame is held back until the next one arrives, so that identical frames can be merged.
/// Call [`GifWriter::finish`] to write the last frame; dropping the writer without it loses it.
pub struct GifWriter {
    /// The `gif` crate encoder writing to the output file.
    encoder: gif::Encoder<BufWriter<File>>,
    /// Frame width in pixels, as stored in the GIF header.
    width: u16,
    /// Frame height in pixels, as stored in the GIF header.
    height: u16,
    /// The most recent frame (RGBA) and its accumulated duration in seconds, not yet written
    /// because the following frames may still be identical to it.
    pending: Option<(Vec<u8>, f64)>,
    /// Rounding error (in 1/100 s) carried from frame to frame so the total length stays exact.
    carry: f64,
}

impl GifWriter {
    /// Creates the GIF file at `path` with the given frame size and infinite looping.
    ///
    /// Dimensions above 65535 are clamped, as GIF stores them as 16-bit values. Fails if the file
    /// cannot be created or the header cannot be written.
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self> {
        let file = BufWriter::new(
            File::create(path).with_context(|| format!("creating {}", path.display()))?,
        );
        let (w, h) = (
            width.min(u16::MAX as u32) as u16,
            height.min(u16::MAX as u32) as u16,
        );
        let mut encoder = gif::Encoder::new(file, w, h, &[]).context("starting GIF")?;
        encoder.set_repeat(gif::Repeat::Infinite)?;
        Ok(Self {
            encoder,
            width: w,
            height: h,
            pending: None,
            carry: 0.0,
        })
    }

    /// Adds an RGBA frame (`width * height * 4` bytes) that is shown for `seconds`.
    ///
    /// A frame identical to the previous one only extends that frame's duration; otherwise the
    /// previous frame is encoded and written, and this one becomes pending.
    pub fn add_rgba(&mut self, rgba: Vec<u8>, seconds: f64) -> Result<()> {
        // Merge with the pending frame when nothing changed on screen.
        if let Some((prev, dur)) = &mut self.pending {
            if *prev == rgba {
                *dur += seconds;
                return Ok(());
            }
        }
        if let Some((prev, dur)) = self.pending.take() {
            self.write(prev, dur)?;
        }
        self.pending = Some((rgba, seconds));
        Ok(())
    }

    /// Quantises and writes one frame with a delay of `seconds`.
    fn write(&mut self, mut rgba: Vec<u8>, seconds: f64) -> Result<()> {
        // GIF delays are in 1/100 s and browsers show delays below 2 as 10, so round to at least
        // 2 and carry the rounding error into the next frame.
        let exact = seconds * 100.0 + self.carry;
        let delay = exact.round().max(2.0);
        self.carry = exact - delay;
        // Speed 10 trades a little palette quality for much faster NeuQuant quantisation.
        let mut frame = gif::Frame::from_rgba_speed(self.width, self.height, &mut rgba, 10);
        frame.delay = delay.min(u16::MAX as f64) as u16;
        self.encoder
            .write_frame(&frame)
            .context("writing GIF frame")?;
        Ok(())
    }

    /// Writes the pending last frame and closes the file (the trailer is written on drop).
    pub fn finish(mut self) -> Result<()> {
        if let Some((prev, dur)) = self.pending.take() {
            self.write(prev, dur)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two identical frames are merged into one with the summed delay; a different frame follows.
    #[test]
    fn writes_and_merges_frames() {
        let path = std::env::temp_dir().join(format!("sr-gif-{}.gif", std::process::id()));
        let mut g = GifWriter::create(&path, 4, 4).unwrap();
        let red: Vec<u8> = [255u8, 0, 0, 255].repeat(16);
        let blue: Vec<u8> = [0u8, 0, 255, 255].repeat(16);
        g.add_rgba(red.clone(), 0.1).unwrap();
        g.add_rgba(red, 0.1).unwrap();
        g.add_rgba(blue, 0.1).unwrap();
        g.finish().unwrap();
        let mut dec = gif::DecodeOptions::new()
            .read_info(std::fs::File::open(&path).unwrap())
            .unwrap();
        let mut delays = vec![];
        while let Some(f) = dec.read_next_frame().unwrap() {
            delays.push(f.delay);
        }
        assert_eq!(delays, vec![20, 10]);
        let _ = std::fs::remove_file(path);
    }
}
