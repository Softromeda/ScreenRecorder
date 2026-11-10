// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Windows Media Foundation wrappers: H.264 / AAC / MP3 encoding, decoding, remuxing and webcam
//! capture.
//!
//! * [`Writer`] writes MP4 (H.264 + AAC, optionally fragmented), M4A or MP3 files from NV12
//!   frames, compressed video samples and 16-bit PCM.
//! * [`VideoReader`] and [`AudioReader`] decode files for the editor and for export;
//!   [`probe`] reads basic stream information.
//! * [`Camera`] and [`list_cameras`] provide webcam frames for the picture-in-picture overlay.
//!
//! These are system components, so nothing needs to be installed; hardware encoders (NVIDIA,
//! Intel, AMD) are used automatically when present and allowed. Every entry point calls [`init`],
//! so COM and Media Foundation are ready on whichever thread uses them.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, HSTRING, Interface, PWSTR};

use super::MediaInfo;

/// 100-nanosecond units per second (Media Foundation time base).
pub const HNS: f64 = 10_000_000.0;

/// Converts seconds to Media Foundation's 100-nanosecond units, rounded to the nearest unit.
pub fn to_hns(seconds: f64) -> i64 {
    (seconds * HNS).round() as i64
}

/// Initialises COM (multithreaded) for the calling thread and Media Foundation for the process.
///
/// Safe to call repeatedly from any thread: `MFStartup` runs once and its result is cached.
/// Fails with an explanatory message on systems without Media Foundation (Windows N/KN editions
/// without the Media Feature Pack).
pub fn init() -> Result<()> {
    // Ignore "already initialised in another mode" (e.g. the UI thread, which uses STA).
    // SAFETY: plain COM initialisation for this thread; takes no pointers.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    // SAFETY: `MFStartup` takes no pointers; the `OnceLock` makes it run once per process.
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| e.to_string()) })
        .clone()
        .map_err(|e| {
            anyhow!(
                "Windows Media Foundation is not available ({e}). On Windows N/KN editions install \
                 the \"Media Feature Pack\" from Settings > Apps > Optional features."
            )
        })
}

/// Packs two 32-bit values into one 64-bit attribute value (as `MFSetAttributeSize` does), e.g.
/// width and height or a frame-rate fraction.
fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Splits a 64-bit attribute value into its high and low 32-bit halves; the inverse of [`pack`].
fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Creates an empty attribute store with room for `count` entries.
fn attributes(count: u32) -> Result<IMFAttributes> {
    let mut attrs = None;
    // SAFETY: `attrs` is a local out-parameter.
    unsafe { MFCreateAttributes(&mut attrs, count)? };
    attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))
}

/// Returns a frame rate (clamped to 1..=240 fps) as a numerator/denominator pair.
///
/// Whole rates use a denominator of 1; others are expressed in 1/1001 units so that NTSC rates
/// such as 29.97 become the exact 30000/1001.
pub fn fps_fraction(fps: f64) -> (u32, u32) {
    let fps = fps.clamp(1.0, 240.0);
    if (fps - fps.round()).abs() < 1e-3 {
        (fps.round() as u32, 1)
    } else {
        ((fps * 1001.0).round() as u32, 1001)
    }
}

// ---------------------------------------------------------------------------
// Media types

/// Output media type for the H.264 encoder: progressive, square pixels, High profile, at the
/// given size, frame rate and average bitrate (bits/s).
fn h264_type(width: u32, height: u32, fps: f64, bitrate: u32) -> Result<IMFMediaType> {
    let (n, d) = fps_fraction(fps);
    // SAFETY: attribute setters on a media type created here; all arguments are values or
    // references to statics.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(n, d))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        Ok(t)
    }
}

/// Input media type for the H.264 encoder: progressive NV12 frames at the given size and frame
/// rate, tightly packed (stride = width) and tagged as BT.709 to match
/// [`convert::bgra_to_nv12`](super::convert::bgra_to_nv12).
fn nv12_type(width: u32, height: u32, fps: f64) -> Result<IMFMediaType> {
    let (n, d) = fps_fraction(fps);
    // SAFETY: as in `h264_type`, attribute setters on a media type created here.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(n, d))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        Ok(t)
    }
}

/// Compressed audio codecs that [`Writer`] can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC-LC, for MP4 and M4A files.
    Aac,
    /// MPEG-1 Layer III, for audio-only MP3 files.
    Mp3,
}

/// Output media type for the audio encoder: stereo at [`RATE`](super::audio::RATE), 192 kbit/s.
fn audio_out_type(codec: AudioCodec) -> Result<IMFMediaType> {
    let rate = super::audio::RATE;
    // SAFETY: attribute setters on a media type created here.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        match codec {
            AudioCodec::Aac => {
                t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                // The AAC encoder requires the input bit depth; 0x29 is AAC profile level 2.
                t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            }
            AudioCodec::Mp3 => t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_MP3)?,
        }
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        // 192 kbit/s (24 000 bytes per second).
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
        if codec == AudioCodec::Mp3 {
            t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1)?;
        }
        Ok(t)
    }
}

/// Input media type for the audio encoder: interleaved 16-bit stereo PCM at
/// [`RATE`](super::audio::RATE).
fn pcm16_type() -> Result<IMFMediaType> {
    let rate = super::audio::RATE;
    // SAFETY: attribute setters on a media type created here.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 4)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        Ok(t)
    }
}

/// Wraps a copy of `bytes` in a new Media Foundation sample with the given timestamp and
/// duration (100 ns units; the duration is at least 1).
fn sample_from_bytes(bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    // SAFETY: the buffer was created with exactly `bytes.len()` bytes of capacity, and the
    // pointer is only written between Lock and Unlock.
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration.max(1))?;
        Ok(sample)
    }
}

// ---------------------------------------------------------------------------
// Writer

/// Parameters of an H.264 video stream to encode.
#[derive(Debug, Clone, Copy)]
pub struct VideoSpec {
    /// Frame width in pixels (even, as required by NV12).
    pub width: u32,
    /// Frame height in pixels (even, as required by NV12).
    pub height: u32,
    /// Nominal frame rate in frames per second.
    pub fps: f64,
    /// Average bitrate in bits per second (see [`video_bitrate`](super::video_bitrate)).
    pub bitrate: u32,
}

/// How the video stream of a [`Writer`] is fed.
pub enum VideoInput<'a> {
    /// Encode raw NV12 frames to H.264.
    Encode(VideoSpec),
    /// Copy already compressed samples of this type (remux, no re-encode).
    Passthrough(&'a IMFMediaType),
}

/// Streams and container options for [`Writer::create`].
pub struct WriterOptions<'a> {
    /// The video stream, or `None` for an audio-only file.
    pub video: Option<VideoInput<'a>>,
    /// The audio codec, or `None` for a file without sound.
    pub audio: Option<AudioCodec>,
    /// Allow hardware (GPU) encoders.
    pub hardware: bool,
    /// Write fragmented MP4, which stays readable if the program crashes or the power fails
    /// (used for recordings in progress).
    pub fragmented: bool,
}

/// Writes MP4 (H.264 + AAC), M4A (AAC) or MP3 files with a Media Foundation sink writer.
///
/// The container follows the file extension (`.mp3`) and [`WriterOptions::fragmented`]. Call
/// [`Writer::finish`] to finalise the file; without it the file is incomplete.
pub struct Writer {
    /// The Media Foundation sink writer that encodes and multiplexes all streams.
    sink: IMFSinkWriter,
    /// Sink stream index of the video stream, if any.
    video: Option<u32>,
    /// Sink stream index of the audio stream, if any.
    audio: Option<u32>,
    /// Audio frames written so far; the next audio timestamp is derived from it.
    audio_written: i64,
}

impl Writer {
    /// Creates (replacing any existing file) the output file at `path` with the requested streams
    /// and starts writing.
    ///
    /// Fails when Media Foundation is unavailable, the file cannot be created, or no installed
    /// encoder accepts the requested format (e.g. a video size the H.264 encoder rejects).
    pub fn create(path: &Path, opts: WriterOptions<'_>) -> Result<Self> {
        init()?;
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        // SAFETY: COM calls on objects created here; all arguments outlive the calls.
        unsafe {
            // Sink writer attributes: hardware encoders and the container type.
            let attrs = attributes(4)?;
            attrs.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                opts.hardware as u32,
            )?;
            let container = if ext == "mp3" {
                MFTranscodeContainerType_MP3
            } else if opts.fragmented {
                MFTranscodeContainerType_FMPEG4
            } else {
                MFTranscodeContainerType_MPEG4
            };
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &container)?;

            // The sink writer does not always overwrite an existing file.
            let _ = std::fs::remove_file(path);
            let sink = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)
                .with_context(|| format!("creating {}", path.display()))?;

            // Video stream: either NV12 in → H.264 out, or the compressed type in and out.
            let mut video = None;
            match opts.video {
                Some(VideoInput::Encode(spec)) => {
                    let out = h264_type(spec.width, spec.height, spec.fps, spec.bitrate)?;
                    let idx = sink.AddStream(&out).context("adding H.264 stream")?;
                    let input = nv12_type(spec.width, spec.height, spec.fps)?;
                    sink.SetInputMediaType(idx, &input, None)
                        .context("no H.264 encoder accepts this video size")?;
                    video = Some(idx);
                }
                Some(VideoInput::Passthrough(t)) => {
                    let idx = sink.AddStream(t).context("adding video stream")?;
                    sink.SetInputMediaType(idx, t, None)
                        .context("copying video stream")?;
                    video = Some(idx);
                }
                None => {}
            }
            // Audio stream: 16-bit PCM in, AAC or MP3 out.
            let mut audio = None;
            if let Some(codec) = opts.audio {
                let out = audio_out_type(codec)?;
                let idx = sink.AddStream(&out).context("adding audio stream")?;
                sink.SetInputMediaType(idx, &pcm16_type()?, None)
                    .with_context(|| format!("no {codec:?} encoder available"))?;
                audio = Some(idx);
            }
            sink.BeginWriting().context("starting the encoder")?;
            Ok(Self {
                sink,
                video,
                audio,
                audio_written: 0,
            })
        }
    }

    /// Encodes one NV12 frame (in the size given at creation) shown at `time` for `duration`,
    /// both in seconds.
    ///
    /// Fails if the writer has no video stream or the encoder reports an error.
    pub fn write_nv12(&self, nv12: &[u8], time: f64, duration: f64) -> Result<()> {
        let idx = self.video.ok_or_else(|| anyhow!("no video stream"))?;
        let sample = sample_from_bytes(nv12, to_hns(time), to_hns(duration))?;
        // SAFETY: `idx` is a stream of this sink writer and `sample` is a valid sample.
        unsafe {
            self.sink
                .WriteSample(idx, &sample)
                .context("encoding video")?
        };
        Ok(())
    }

    /// Writes a compressed video sample unchanged (from [`VideoReader::read_compressed`]), for a
    /// writer created with [`VideoInput::Passthrough`]. The sample keeps its own timestamps.
    pub fn write_video_sample(&self, sample: &IMFSample) -> Result<()> {
        let idx = self.video.ok_or_else(|| anyhow!("no video stream"))?;
        // SAFETY: `idx` is a stream of this sink writer and `sample` is a valid sample.
        unsafe {
            self.sink
                .WriteSample(idx, sample)
                .context("writing video")?
        };
        Ok(())
    }

    /// Appends interleaved 16-bit stereo samples at [`RATE`](super::audio::RATE).
    ///
    /// Timestamps continue from the previously written audio, so the audio track is contiguous
    /// and starts at time 0.
    pub fn write_audio(&mut self, samples: &[i16]) -> Result<()> {
        let idx = self.audio.ok_or_else(|| anyhow!("no audio stream"))?;
        // Write in 100 ms samples so the muxer can interleave audio and video finely.
        for chunk in samples.chunks(super::audio::RATE as usize / 10 * 2) {
            // Derive start and end (100 ns units) from the frame count so rounding never
            // accumulates.
            let frames = (chunk.len() / 2) as i64;
            let time = self.audio_written * 10_000_000 / super::audio::RATE as i64;
            let end = (self.audio_written + frames) * 10_000_000 / super::audio::RATE as i64;
            let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
            let sample = sample_from_bytes(&bytes, time, end - time)?;
            // SAFETY: `idx` is a stream of this sink writer and `sample` is a valid sample.
            unsafe {
                self.sink
                    .WriteSample(idx, &sample)
                    .context("encoding audio")?
            };
            self.audio_written += frames;
        }
        Ok(())
    }

    /// Flushes the encoders and finalises the file (writes the index for non-fragmented MP4).
    pub fn finish(self) -> Result<()> {
        // SAFETY: finalising a sink writer we own; it is consumed and not used afterwards.
        unsafe { self.sink.Finalize().context("finishing the file")? };
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Readers

// Source reader pseudo stream indices, as the `u32` the reader methods take.

/// Pseudo stream index for the first video stream of a source reader.
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Pseudo stream index for the first audio stream of a source reader.
const FIRST_AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
/// Pseudo stream index addressing every stream at once (for stream selection).
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
/// Pseudo stream index for the media source itself (for presentation attributes).
const MEDIASOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;

/// Opens a source reader for a file.
///
/// With `video_processing`, the reader may insert Media Foundation's video processor so that it
/// can deliver RGB32 instead of the decoder's native output.
fn open_reader(path: &Path, video_processing: bool) -> Result<IMFSourceReader> {
    init()?;
    // SAFETY: COM calls on an attribute store created here; the path string outlives the call.
    unsafe {
        let attrs = attributes(2)?;
        if video_processing {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        }
        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
            .with_context(|| format!("cannot open {}", path.display()))
    }
}

/// Seeks a source reader to `seconds` (negative values seek to the start).
///
/// Media Foundation seeks to the nearest preceding key frame at or before the position, so the
/// caller must skip decoded frames until the wanted time.
fn seek_reader(reader: &IMFSourceReader, seconds: f64) -> Result<()> {
    let mut pos = PROPVARIANT::default();
    // SAFETY: the PROPVARIANT is initialised as a VT_I8 by setting its tag and value together.
    unsafe {
        let inner = &mut *pos.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = to_hns(seconds.max(0.0));
        reader.SetCurrentPosition(&GUID::zeroed(), &pos)?;
    }
    Ok(())
}

/// Reads the duration, frame size, frame rate and stream presence of a media file.
///
/// A missing or invalid frame rate is reported as 30 fps. Fails if the file cannot be opened or
/// contains neither audio nor video that Windows can read.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let reader = open_reader(path, false)?;
    let mut info = MediaInfo::default();
    // SAFETY: reader queries with local out-values; the duration union member is read only
    // after checking that the PROPVARIANT holds a 64-bit integer.
    unsafe {
        if let Ok(pv) = reader.GetPresentationAttribute(MEDIASOURCE, &MF_PD_DURATION) {
            let inner = &*pv.Anonymous.Anonymous;
            if inner.vt == VT_UI8 || inner.vt == VT_I8 {
                info.duration = inner.Anonymous.uhVal as f64 / HNS;
            }
        }
        if let Ok(t) = reader.GetNativeMediaType(FIRST_VIDEO, 0) {
            info.has_video = true;
            if let Ok(size) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                (info.width, info.height) = unpack(size);
            }
            if let Ok(rate) = t.GetUINT64(&MF_MT_FRAME_RATE) {
                let (n, d) = unpack(rate);
                if n > 0 && d > 0 {
                    info.fps = n as f64 / d as f64;
                }
            }
        }
        info.has_audio = reader.GetNativeMediaType(FIRST_AUDIO, 0).is_ok();
    }
    if info.fps <= 0.0 || !info.fps.is_finite() {
        info.fps = 30.0;
    }
    if !info.has_video && !info.has_audio {
        bail!(
            "{} contains no audio or video Windows can read",
            path.display()
        );
    }
    Ok(info)
}

/// A decoded video frame: top-down, tightly packed BGRA with opaque alpha.
pub struct Frame {
    /// Presentation time in seconds.
    pub time: f64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes of BGRA pixels.
    pub bgra: Vec<u8>,
}

/// Decodes the first video stream of a file (or a webcam) to BGRA frames, or reads its
/// compressed samples for remuxing.
pub struct VideoReader {
    /// The source reader, with only the first video stream selected.
    reader: IMFSourceReader,
    /// Visible frame width in pixels.
    pub width: u32,
    /// Visible frame height in pixels.
    pub height: u32,
    /// Decoded buffer width in pixels (may be padded beyond [`VideoReader::width`]).
    buf_width: u32,
    /// Decoded buffer height in pixels (may be padded, e.g. 1088 rows for 1080p NV12).
    buf_height: u32,
    /// Default stride of the decoded buffer in bytes; negative for bottom-up images.
    stride: i32,
    /// Whether samples are decoded (false: compressed samples for passthrough).
    decode: bool,
    /// Decoding to NV12 and converting in Rust (fast mode, or when RGB32 is unavailable).
    nv12: bool,
}

impl VideoReader {
    /// Opens the first video stream of a file. With `decode == false` the samples stay
    /// compressed (for [`VideoReader::read_compressed`]); otherwise frames are decoded to BGRA.
    pub fn open(path: &Path, decode: bool) -> Result<Self> {
        let reader = open_reader(path, decode)?;
        Self::from_reader(reader, decode)
    }

    /// Opens a file for fast decoding: the decoder's NV12 output is converted to BGRA on all CPU
    /// cores instead of by Media Foundation's single-threaded video processor. Used for real-time
    /// preview playback and library thumbnails.
    pub fn open_fast(path: &Path) -> Result<Self> {
        let reader = open_reader(path, false)?;
        Self::from_reader_with(reader, true, true)
    }

    /// Wraps a source reader, decoding through the video processor when `decode` is set.
    fn from_reader(reader: IMFSourceReader, decode: bool) -> Result<Self> {
        Self::from_reader_with(reader, decode, false)
    }

    /// Wraps a source reader: selects only its first video stream, chooses the output format
    /// (RGB32, or NV12 when `prefer_nv12` is set or RGB32 is unavailable) and reads the frame
    /// geometry.
    fn from_reader_with(reader: IMFSourceReader, decode: bool, prefer_nv12: bool) -> Result<Self> {
        // Deselect everything but the first video stream so the reader does not queue unread
        // samples of other streams.
        // SAFETY: stream selection on a reader we own, using the pseudo stream indices.
        unsafe {
            reader.SetStreamSelection(ALL_STREAMS, false)?;
            reader
                .SetStreamSelection(FIRST_VIDEO, true)
                .context("the file has no video")?;
        }
        let mut nv12 = false;
        if decode {
            // Prefer RGB32 (BGRA) from Media Foundation's video processor;
            // fall back to the decoder's NV12 output and convert ourselves.
            let set = |subtype: &GUID| -> windows::core::Result<()> {
                // SAFETY: builds a partial media type locally and passes it to the reader.
                unsafe {
                    let t = MFCreateMediaType()?;
                    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                    reader.SetCurrentMediaType(FIRST_VIDEO, None, &t)
                }
            };
            if prefer_nv12 {
                set(&MFVideoFormat_NV12).context("Windows cannot decode this video format")?;
                nv12 = true;
            } else {
                match set(&MFVideoFormat_RGB32) {
                    Err(e) => {
                        log::info!("RGB32 decoding unavailable ({e}); decoding to NV12");
                        set(&MFVideoFormat_NV12)
                            .context("Windows cannot decode this video format")?;
                        nv12 = true;
                    }
                    _ => {}
                }
            }
        }
        // Visible size from the file (decoders may pad the buffer).
        // SAFETY: read-only queries on a reader we own.
        let display = unsafe {
            reader
                .GetNativeMediaType(FIRST_VIDEO, 0)
                .and_then(|t| t.GetUINT64(&MF_MT_FRAME_SIZE))
                .map(unpack)
                .ok()
        };
        let mut r = Self {
            reader,
            width: 0,
            height: 0,
            buf_width: 0,
            buf_height: 0,
            stride: 0,
            decode,
            nv12,
        };
        r.refresh_type()?;
        if let Some((w, h)) = display {
            if w > 0 && h > 0 && w <= r.buf_width && h <= r.buf_height {
                (r.width, r.height) = (w, h);
            }
        }
        Ok(r)
    }

    /// Reads the buffer size and stride from the current output type; called at start-up and
    /// whenever the decoder changes its output type mid-stream.
    fn refresh_type(&mut self) -> Result<()> {
        // SAFETY: read-only queries on the reader's current media type.
        unsafe {
            let t = self.reader.GetCurrentMediaType(FIRST_VIDEO)?;
            let (bw, bh) = unpack(t.GetUINT64(&MF_MT_FRAME_SIZE)?);
            // Keep a smaller visible size chosen from the file's native size.
            if self.width == 0 || self.width > bw || self.height > bh {
                (self.width, self.height) = (bw, bh);
            }
            (self.buf_width, self.buf_height) = (bw, bh);
            let format = if self.nv12 {
                MFVideoFormat_NV12
            } else {
                MFVideoFormat_RGB32
            };
            // Not every decoder sets the default stride; compute it from the format otherwise.
            self.stride = match t.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(s) => s as i32,
                Err(_) => MFGetStrideForBitmapInfoHeader(format.data1, bw)
                    .unwrap_or(bw as i32 * if self.nv12 { 1 } else { 4 }),
            };
        }
        Ok(())
    }

    /// Returns the media type of the compressed stream, for [`VideoInput::Passthrough`] (only
    /// meaningful when opened with `decode == false`).
    pub fn native_type(&self) -> Result<IMFMediaType> {
        // SAFETY: read-only query on a reader we own.
        unsafe { Ok(self.reader.GetCurrentMediaType(FIRST_VIDEO)?) }
    }

    /// Seeks to `seconds`; reading resumes at the key frame at or before that time.
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        seek_reader(&self.reader, seconds)
    }

    /// Returns the next sample with its time in seconds, or `None` at the end of the stream.
    ///
    /// Skips reader events that carry no sample (stream ticks, type changes) and refreshes the
    /// frame geometry when the decoder changes its output type.
    fn read_sample(&mut self) -> Result<Option<(f64, IMFSample)>> {
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: synchronous read into local out-parameters.
            unsafe {
                self.reader.ReadSample(
                    FIRST_VIDEO,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )?;
            }
            let flags = flags as i32;
            if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
                bail!("error while reading the video");
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 && self.decode {
                self.refresh_type()?;
            }
            if let Some(s) = sample {
                return Ok(Some((time as f64 / HNS, s)));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                return Ok(None);
            }
        }
    }

    /// Returns the next compressed sample (opened with `decode == false`), or `None` at the end.
    pub fn read_compressed(&mut self) -> Result<Option<IMFSample>> {
        Ok(self.read_sample()?.map(|(_, s)| s))
    }

    /// Returns the next decoded frame as BGRA, or `None` at the end of the stream.
    ///
    /// In NV12 mode the frame size is rounded down to even dimensions; if the buffer is smaller
    /// than expected or bottom-up, a black frame is returned instead of reading out of bounds.
    pub fn read_frame(&mut self) -> Result<Option<Frame>> {
        let Some((time, sample)) = self.read_sample()? else {
            return Ok(None);
        };
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY: rows are only read within the locked buffer: the pitch and base come from
        // Lock2D or the media type, the NV12 path checks the buffer length, and the buffer is
        // unlocked before it is released.
        let bgra = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            // Lock2D gives the true pitch (negative for bottom-up images).
            let (base, pitch, len, locked2d) = match buffer.cast::<IMF2DBuffer>() {
                Ok(b2) => {
                    let mut scan0 = std::ptr::null_mut();
                    let mut pitch = 0i32;
                    b2.Lock2D(&mut scan0, &mut pitch)?;
                    (scan0 as *const u8, pitch as isize, None, Some(b2))
                }
                _ => {
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut ptr, None, Some(&mut len))?;
                    let stride = self.stride as isize;
                    // Without Lock2D a negative stride means bottom-up rows.
                    let base = if stride < 0 {
                        let rows = self.buf_height as isize;
                        (ptr as *const u8).offset(-stride * (rows - 1))
                    } else {
                        ptr as *const u8
                    };
                    (base, stride, Some(len as usize), None)
                }
            };
            let out = if self.nv12 {
                // The UV plane follows the (possibly padded) Y plane at the same pitch; convert
                // the visible, even-sized part on all cores.
                let pitch_abs = pitch.unsigned_abs();
                let rows_y = self.buf_height as usize;
                let total = pitch_abs * (rows_y + rows_y / 2);
                let ok = len.is_none_or(|l| l >= total) && pitch > 0;
                if ok {
                    let src = std::slice::from_raw_parts(base, total);
                    let w2 = w & !1;
                    let h2 = h & !1;
                    crate::media::convert::nv12_to_bgra(
                        src,
                        pitch_abs,
                        pitch_abs * rows_y,
                        w2.max(2),
                        h2.max(2),
                    )
                } else {
                    vec![0u8; w * h * 4]
                }
            } else {
                // RGB32: copy the visible rows (pitch may be negative) and force opaque alpha,
                // since the X8 byte is undefined.
                let row = w * 4;
                let mut out = vec![0u8; row * h];
                for y in 0..h {
                    let src = base.offset(pitch * y as isize);
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
                }
                for px in out.chunks_exact_mut(4) {
                    px[3] = 255;
                }
                out
            };
            match locked2d {
                Some(b2) => b2.Unlock2D()?,
                None => buffer.Unlock()?,
            }
            out
        };
        // NV12 output is rounded down to even dimensions.
        let (fw, fh) = if self.nv12 {
            ((self.width & !1).max(2), (self.height & !1).max(2))
        } else {
            (self.width, self.height)
        };
        Ok(Some(Frame {
            time,
            width: fw,
            height: fh,
            bgra,
        }))
    }
}

/// Decodes the first audio stream of a file to interleaved `f32` samples in its native sample
/// rate and channel count.
pub struct AudioReader {
    /// The source reader, with only the first audio stream selected and float output set.
    reader: IMFSourceReader,
    /// Sample rate of the decoded audio in Hz.
    pub rate: u32,
    /// Number of interleaved channels of the decoded audio.
    pub channels: usize,
}

impl AudioReader {
    /// Opens the first audio stream of a file and asks the decoder for 32-bit float output.
    ///
    /// Fails if the file has no audio or Windows has no decoder for its format.
    pub fn open(path: &Path) -> Result<Self> {
        let reader = open_reader(path, false)?;
        // SAFETY: stream selection and media type calls on a reader created here; the partial
        // media type is a local COM object.
        unsafe {
            reader.SetStreamSelection(ALL_STREAMS, false)?;
            reader
                .SetStreamSelection(FIRST_AUDIO, true)
                .context("the file has no audio")?;
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            reader
                .SetCurrentMediaType(FIRST_AUDIO, None, &t)
                .context("Windows cannot decode this audio format")?;
            let cur = reader.GetCurrentMediaType(FIRST_AUDIO)?;
            let rate = cur.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let channels = cur.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? as usize;
            Ok(Self {
                reader,
                rate,
                channels,
            })
        }
    }

    /// Seeks to `seconds`; the next chunk may start slightly before that time.
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        seek_reader(&self.reader, seconds)
    }

    /// Returns the next decoded chunk as `(start time in seconds, interleaved samples)`, or
    /// `None` at the end of the stream.
    pub fn read_chunk(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: synchronous read into local out-parameters.
            unsafe {
                self.reader.ReadSample(
                    FIRST_AUDIO,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )?;
            }
            let flags = flags as i32;
            if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
                bail!("error while reading the audio");
            }
            if let Some(s) = sample {
                let mut out = vec![];
                // SAFETY: the locked buffer holds `len` bytes of 32-bit float samples; only
                // whole samples within it are copied before unlocking.
                unsafe {
                    let buffer = s.ConvertToContiguousBuffer()?;
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut ptr, None, Some(&mut len))?;
                    let n = len as usize / 4;
                    out.extend_from_slice(std::slice::from_raw_parts(ptr as *const f32, n));
                    buffer.Unlock()?;
                }
                return Ok(Some((time as f64 / HNS, out)));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                return Ok(None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Webcams

/// Enumerates video capture devices as activation objects.
fn enum_cameras() -> Result<Vec<IMFActivate>> {
    init()?;
    // SAFETY: `list` and `count` are written by `MFEnumDeviceSources`; only the first `count`
    // entries of the returned array are read, and the array is freed exactly once.
    unsafe {
        let attrs = attributes(1)?;
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut list, &mut count)?;
        // The array is allocated with CoTaskMemAlloc: take ownership of each entry, then free
        // the array itself.
        let mut out = vec![];
        if !list.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*list.add(i)).take() {
                    out.push(a);
                }
            }
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

/// Returns a capture device's display name, or "Camera" if it has none.
fn friendly_name(a: &IMFActivate) -> String {
    // SAFETY: the string is allocated by Media Foundation with CoTaskMemAlloc and freed once
    // after being copied.
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut s, &mut len)
            .is_ok()
        {
            let name = s.to_string().unwrap_or_default();
            CoTaskMemFree(Some(s.0 as *const _));
            name
        } else {
            "Camera".into()
        }
    }
}

/// Returns the display names of all connected webcams (empty on error), for webcam selection.
pub fn list_cameras() -> Vec<String> {
    enum_cameras()
        .map(|list| list.iter().map(friendly_name).collect())
        .unwrap_or_default()
}

/// A webcam delivering BGRA frames, used for the picture-in-picture overlay while recording.
pub struct Camera {
    /// Decoding reader on the webcam's media source, delivering RGB32 frames.
    reader: VideoReader,
}

impl Camera {
    /// Opens the webcam with this display name, or the first one when `name` is `None` or not
    /// found. Fails when no webcam is connected or it cannot be opened (e.g. in use or blocked by
    /// privacy settings).
    pub fn open(name: Option<&str>) -> Result<Self> {
        let cams = enum_cameras()?;
        let activate = cams
            .iter()
            .find(|a| name.is_some_and(|n| friendly_name(a) == n))
            .or_else(|| cams.first())
            .ok_or_else(|| anyhow!("no webcam found"))?;
        // Activate the device and read it through a source reader with the video processor,
        // so frames arrive as RGB32.
        // SAFETY: COM calls on the activation object and on objects created here.
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject().context("opening the webcam")?;
            let attrs = attributes(1)?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
            Ok(Self {
                reader: VideoReader::from_reader(reader, true)?,
            })
        }
    }

    /// Blocks until the camera delivers its next frame and returns it as BGRA.
    pub fn read(&mut self) -> Result<Option<Frame>> {
        self.reader.read_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole and NTSC frame rates map to exact fractions, and size packing round-trips.
    #[test]
    fn fractions() {
        assert_eq!(fps_fraction(60.0), (60, 1));
        assert_eq!(fps_fraction(29.97), (30000, 1001));
        assert_eq!(unpack(pack(1920, 1080)), (1920, 1080));
    }
}
