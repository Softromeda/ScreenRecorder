// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Microphone and speaker capture to WAV files using `cpal` (WASAPI).
//!
//! [`AudioCapture`] records one source into a 32-bit float WAV file at the device's native rate
//! and channel count; speakers are captured with WASAPI loopback on the output device. Samples are
//! placed on the shared [`ActiveClock`] timeline (pauses are skipped, gaps are filled with
//! silence) so the audio lines up with the video. [`Level`] feeds the live meters on the control
//! bar, and the `list_*` functions enumerate devices for the Options window and `--list-devices`.
//! Mixing, resampling and encoding happen later, in [`crate::compose`].

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};

use crate::clock::ActiveClock;

/// WAV file writer used for the raw audio tracks.
type Writer = hound::WavWriter<BufWriter<File>>;

/// Which kind of audio is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// An input device (microphone, line in).
    Microphone,
    /// System audio, captured by WASAPI loopback on an output device.
    Speakers,
}

/// Shared peak level (0..=1) for the on-screen audio meter.
///
/// The audio callback raises it with [`Level::set_max`]; the UI reads and decays it with
/// [`Level::take`]. Clones share the same value; the `f32` is stored as its bit pattern in an
/// atomic so no lock is needed in the audio callback.
#[derive(Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    /// Raises the stored peak to `v` if `v` is higher.
    ///
    /// The load and store are not one atomic step, which is acceptable for a level meter.
    pub fn set_max(&self, v: f32) {
        let cur = f32::from_bits(self.0.load(Ordering::Relaxed));
        if v > cur {
            self.0.store(v.to_bits(), Ordering::Relaxed);
        }
    }

    /// Returns the current peak and decays it to 60%, so the meter falls smoothly between
    /// audio buffers.
    pub fn take(&self) -> f32 {
        let v = f32::from_bits(self.0.load(Ordering::Relaxed));
        self.0.store((v * 0.6).to_bits(), Ordering::Relaxed);
        v
    }
}

/// A running capture of one audio source into a WAV file.
///
/// Created by [`AudioCapture::start`]; call [`AudioCapture::finish`] to stop and finalise the
/// file. Dropping it without `finish` stops the stream but leaves the WAV header unfinalised.
pub struct AudioCapture {
    /// The running capture (a device stream or one program's sound); recording stops when it
    /// is dropped.
    stream: Option<Backend>,
    /// The WAV writer, shared with the audio callback; `None` once finalised.
    writer: Arc<Mutex<Option<Writer>>>,
    /// Live peak level of this source.
    pub level: Level,
}

/// Where a capture's audio comes from. The values are only held, never read: keeping them
/// alive keeps the capture running, and dropping them stops it.
#[allow(dead_code)]
enum Backend {
    /// A cpal stream on an input device, or loopback on an output device.
    Device(cpal::Stream),
    /// One program's sound (WASAPI process loopback).
    Program(crate::media::apploop::AppCapture),
}

/// Display name of a cpal device, or "Unknown device" if it cannot be read.
fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| "Unknown device".into())
}

/// Names of all input devices (microphones), for the device picker.
pub fn list_microphones() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| devs.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

/// Every input device name with its cpal device id in brackets, for `--list-devices`
/// diagnostics.
pub fn list_all_inputs() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| {
            devs.map(|d| {
                let id = d.id().map(|i| i.to_string()).unwrap_or_default();
                format!("{} [{id}]", device_name(&d))
            })
            .collect()
        })
        .unwrap_or_default()
}

/// Names of all output devices, whose sound can be recorded by loopback ("speakers").
pub fn list_speakers() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devs| devs.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

/// Finds the device named `wanted` for `source`, falling back to the system default device
/// when no name is given or no device has that name (e.g. it was unplugged).
///
/// Fails only if there is no default device either.
fn find_device(source: AudioSource, wanted: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    // Look a device up by its display name; `None` if no name was requested.
    let by_name = |devs: Box<dyn Iterator<Item = cpal::Device>>| -> Option<cpal::Device> {
        let wanted = wanted?;
        devs.into_iter().find(|d| device_name(d) == wanted)
    };
    match source {
        AudioSource::Microphone => {
            if let Some(d) = host.input_devices().ok().and_then(|d| by_name(Box::new(d))) {
                return Ok(d);
            }
            host.default_input_device()
                .ok_or_else(|| anyhow!("No microphone found"))
        }
        AudioSource::Speakers => {
            if let Some(d) = host
                .output_devices()
                .ok()
                .and_then(|d| by_name(Box::new(d)))
            {
                return Ok(d);
            }
            host.default_output_device()
                .ok_or_else(|| anyhow!("No speaker/output device found"))
        }
    }
}

/// State used by the audio callback of one [`AudioCapture`].
struct CallbackState {
    /// The WAV writer, shared with [`AudioCapture::finish`].
    writer: Arc<Mutex<Option<Writer>>>,
    /// Shared recording clock; samples are only written while it runs.
    clock: ActiveClock,
    /// Peak meter updated with every buffer.
    level: Level,
    /// Linear gain applied to every sample before clamping to -1..=1.
    gain: f32,
    /// Number of interleaved channels (at least 1).
    channels: usize,
    /// Device sample rate in Hz.
    rate: f64,
    /// Sample frames (one sample per channel) written so far, including padding.
    frames_written: u64,
    /// When the WAV writer was last flushed.
    last_flush: Instant,
}

impl CallbackState {
    /// Handles one buffer of interleaved samples from a cpal device stream.
    ///
    /// Converts the samples to `f32`, works out how long ago they were captured from the
    /// callback's timestamps and hands them to [`CallbackState::push`].
    fn process<T>(&mut self, data: &[T], info: &cpal::InputCallbackInfo)
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        // Where on the recording timeline the first sample of this buffer belongs: now minus
        // the time the data spent in device buffers (capped at 3 s against bogus timestamps).
        let ts = info.timestamp();
        let latency = ts
            .callback
            .saturating_duration_since(ts.capture)
            .as_secs_f64()
            .min(3.0);
        let samples: Vec<f32> = data
            .iter()
            .map(|&s| <f32 as cpal::FromSample<T>>::from_sample_(s))
            .collect();
        self.push(&samples, latency);
    }

    /// Appends one buffer of interleaved `f32` samples captured `latency` seconds ago.
    ///
    /// Drops the data while the clock is paused, pads with silence when the file has fallen
    /// behind the recording timeline, then applies the gain, updates the peak level and appends
    /// the samples. Write errors are ignored (audio callbacks cannot report them).
    fn push(&mut self, data: &[f32], latency: f64) {
        if !self.clock.is_running() {
            return; // paused (or not yet started)
        }
        let mut guard = self.writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let frames = (data.len() / self.channels) as u64;

        // Loopback sources deliver nothing while silent, and devices take a moment to start:
        // pad with silence so the audio stays in sync with the video timeline. Gaps up to
        // 60 ms are ignored to avoid reacting to timing jitter.
        let start = (self.clock.elapsed_secs() - latency).max(0.0);
        let expected = (start * self.rate) as u64;
        let behind = expected.saturating_sub(self.frames_written);
        if behind as f64 > self.rate * 0.06 {
            for _ in 0..behind * self.channels as u64 {
                let _ = writer.write_sample(0.0f32);
            }
            self.frames_written += behind;
        }

        // Apply the gain, clamp, track the peak and append.
        let mut peak = 0.0f32;
        for &s in data {
            let v = (s * self.gain).clamp(-1.0, 1.0);
            peak = peak.max(v.abs());
            let _ = writer.write_sample(v);
        }
        self.frames_written += frames;
        self.level.set_max(peak);

        if self.last_flush.elapsed().as_secs_f32() > 1.0 {
            // Flush about once a second so the WAV header stays valid if the program crashes.
            let _ = writer.flush();
            self.last_flush = Instant::now();
        }
    }
}

impl AudioCapture {
    /// Starts recording `source` into a new WAV file at `path`.
    ///
    /// `wanted` is the device name (or `None` for the default device), `clock` the shared
    /// recording clock and `gain` a linear volume factor. The file uses the device's default
    /// sample rate and channel count, stored as 32-bit float. Fails if no device is found, it
    /// has no usable configuration or sample format, or the file or stream cannot be created.
    pub fn start(
        source: AudioSource,
        wanted: Option<&str>,
        path: &Path,
        clock: ActiveClock,
        gain: f32,
    ) -> Result<Self> {
        let device = find_device(source, wanted)?;
        let name = device_name(&device);
        // Loopback uses the output device's mix format; microphones their input format.
        let config = match source {
            AudioSource::Speakers => device.default_output_config(),
            _ => device.default_input_config(),
        }
        .with_context(|| format!("audio device '{name}' has no usable configuration"))?;

        let spec = hound::WavSpec {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = Arc::new(Mutex::new(Some(
            hound::WavWriter::create(path, spec).context("creating WAV file")?,
        )));
        let level = Level::default();
        let state = CallbackState {
            writer: writer.clone(),
            clock,
            level: level.clone(),
            gain,
            channels: config.channels().max(1) as usize,
            rate: config.sample_rate() as f64,
            frames_written: 0,
            last_flush: Instant::now(),
        };

        let err_fn = move |e: cpal::Error| log::warn!("audio stream error: {e}");
        let format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();

        // Build an input stream for the device's native sample type; for an output device cpal's
        // WASAPI backend opens it in loopback mode. The macro instantiates the callback for each
        // supported sample type.
        let state = Arc::new(Mutex::new(state));
        let build = |cfg: cpal::StreamConfig| {
            macro_rules! build {
                ($t:ty) => {{
                    let st = state.clone();
                    device.build_input_stream(
                        cfg,
                        move |data: &[$t], info: &cpal::InputCallbackInfo| {
                            st.lock().unwrap().process(data, info)
                        },
                        err_fn,
                        None,
                    )
                }};
            }
            match format {
                SampleFormat::I8 => build!(i8),
                SampleFormat::I16 => build!(i16),
                SampleFormat::I32 => build!(i32),
                SampleFormat::U8 => build!(u8),
                SampleFormat::U16 => build!(u16),
                SampleFormat::F32 => build!(f32),
                SampleFormat::F64 => build!(f64),
                other => Err(cpal::Error::with_message(
                    cpal::ErrorKind::UnsupportedConfig,
                    format!("unsupported sample format {other:?}"),
                )),
            }
        };
        let stream =
            build(stream_config).with_context(|| format!("opening audio device '{name}'"))?;
        stream.play().context("starting audio stream")?;

        Ok(Self {
            stream: Some(Backend::Device(stream)),
            writer,
            level,
        })
    }

    /// Starts recording only the sound of the program with process ID `pid` (and the
    /// processes it started) into a new 48 kHz stereo WAV file at `path`.
    ///
    /// Needs Windows 10 version 2004 or newer; fails with the reason otherwise.
    pub fn start_program(pid: u32, path: &Path, clock: ActiveClock, gain: f32) -> Result<Self> {
        let rate = crate::media::apploop::RATE;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = Arc::new(Mutex::new(Some(
            hound::WavWriter::create(path, spec).context("creating WAV file")?,
        )));
        let level = Level::default();
        let mut state = CallbackState {
            writer: writer.clone(),
            clock,
            level: level.clone(),
            gain,
            channels: 2,
            rate: rate as f64,
            frames_written: 0,
            last_flush: Instant::now(),
        };
        let capture =
            crate::media::apploop::AppCapture::start(pid, move |data| state.push(data, 0.0))?;
        Ok(Self {
            stream: Some(Backend::Program(capture)),
            writer,
            level,
        })
    }

    /// Stops capturing and finalises the WAV file (writes the final header).
    pub fn finish(mut self) -> Result<()> {
        // Stop the capture first so no callback writes while the file is finalised.
        drop(self.stream.take());
        if let Some(w) = self.writer.lock().unwrap().take() {
            w.finalize().context("finalising WAV file")?;
        }
        Ok(())
    }
}
