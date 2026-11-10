// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Background frame decoding for the editor preview.
//!
//! [`Preview`] owns a dedicated "editor-preview" thread with its own Media Foundation
//! [`mf::VideoReader`]. The editor UI sends it seek and play requests; the thread decodes the
//! matching frames, converts them into egui images and hands them back through
//! [`Preview::frames`]. Playback is silent: it only drives the picture and the playhead. Every
//! request carries a generation number so the UI can drop frames that belong to a request it has
//! since superseded.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use crate::media::{convert, mf};

/// A decoded preview frame delivered from the preview thread to the UI.
pub struct Frame {
    /// Presentation time of the frame in the source video, in seconds.
    pub time: f64,
    /// RGBA pixels ready to upload as an egui texture (at most [`MAX_WIDTH`] wide).
    pub image: egui::ColorImage,
    /// `true` when the frame comes from continuous playback, `false` for a single seek.
    pub playing: bool,
    /// Playback reached the end of the file; `image` is empty and `time` is meaningless.
    pub eof: bool,
    /// Generation of the request that produced this frame; frames from older generations are
    /// stale and are ignored by the UI.
    pub generation: u64,
}

/// A command sent from the UI to the preview thread.
enum Request {
    /// Decode and deliver the single frame at the given time (seconds, generation).
    Seek(f64, u64),
    /// Play in real time starting at the given time (seconds, generation).
    Play(f64, u64),
    /// Stop playback; the thread goes idle until the next request.
    Stop,
}

/// Handle to the background preview decoder of the open video.
///
/// Dropping the handle stops playback and closes the request channel, which ends the thread.
pub struct Preview {
    /// Request channel to the preview thread.
    tx: Sender<Request>,
    /// Decoded frames from the preview thread. The channel is small (bounded) so that playback
    /// cannot run ahead of the UI.
    pub frames: Receiver<Frame>,
    /// Generation counter, incremented by every request sent from the UI.
    generation: Arc<AtomicU64>,
}

/// Maximum preview width in pixels; wider frames are scaled down to this width. The limit is
/// large enough to stay sharp on 1440p screens while keeping 4K playback affordable.
const MAX_WIDTH: u32 = 2560;

/// Converts a decoded BGRA frame into a preview [`Frame`].
///
/// Runs on the preview thread and uses all CPU cores (rayon) for the scaling and the BGRA to
/// RGBA conversion, so the UI thread only has to upload the finished image.
fn to_preview(f: mf::Frame, playing: bool, generation: u64) -> Frame {
    use rayon::prelude::*;
    let (w, h) = (f.width, f.height);
    // Scale frames wider than MAX_WIDTH down, keeping the aspect ratio.
    let (pw, ph, bgra) = if w > MAX_WIDTH {
        let ph = ((h as f64 * MAX_WIDTH as f64 / w as f64) as u32).max(1);
        let small = convert::resize(
            &f.bgra,
            w as usize,
            h as usize,
            MAX_WIDTH as usize,
            ph as usize,
        );
        (MAX_WIDTH, ph, small)
    } else {
        (w, h, f.bgra)
    };
    // Reorder BGRA bytes into egui colours in parallel; alpha is dropped (frames are opaque).
    let pixels: Vec<egui::Color32> = bgra
        .par_chunks_exact(4)
        .map(|p| egui::Color32::from_rgb(p[2], p[1], p[0]))
        .collect();
    Frame {
        time: f.time,
        image: egui::ColorImage::new([pw as usize, ph as usize], pixels),
        playing,
        eof: false,
        generation,
    }
}

/// Builds the empty end-of-file marker frame sent when playback runs past the last frame.
fn eof(generation: u64) -> Frame {
    Frame {
        time: 0.0,
        image: egui::ColorImage::default(),
        playing: true,
        eof: true,
        generation,
    }
}

impl Preview {
    /// Starts the preview thread for the video at `source`.
    ///
    /// `fps` is the video's frame rate and sets the timing tolerance used when matching frames to
    /// requested times. `ctx` is used to request a UI repaint whenever a frame is delivered. If
    /// Media Foundation or the file cannot be opened, the error is logged and the thread exits,
    /// so the handle simply never delivers frames.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the thread.
    pub fn new(source: PathBuf, fps: f64, ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let (ftx, frx) = crossbeam_channel::bounded::<Frame>(4);
        // Half a frame period is the tolerance for "this frame shows time t"; a full period is
        // how late a playback frame may be before it is dropped.
        let half_frame = 0.5 / fps.clamp(1.0, 120.0);
        let frame_time = Duration::from_secs_f64(2.0 * half_frame);

        std::thread::Builder::new()
            .name("editor-preview".into())
            .spawn(move || {
                // Media Foundation must be initialised on each thread that uses it.
                if let Err(e) = mf::init() {
                    log::error!("preview: {e:#}");
                    return;
                }
                let mut reader = match mf::VideoReader::open_fast(&source) {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("preview: {e:#}");
                        return;
                    }
                };
                // A request that interrupted playback and still has to be handled.
                let mut pending: Option<Request> = None;
                loop {
                    // Wait for the next request; the timeout only keeps the loop responsive and
                    // the thread ends once the UI side drops its sender.
                    let req = match pending.take() {
                        Some(r) => r,
                        None => match rx.recv_timeout(Duration::from_millis(200)) {
                            Ok(r) => r,
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => break,
                        },
                    };
                    // Coalesce queued requests: while scrubbing only the newest one matters.
                    let mut req = req;
                    while let Ok(r) = rx.try_recv() {
                        req = r;
                    }
                    match req {
                        Request::Stop => {}
                        Request::Seek(t, g) => {
                            if reader.seek(t).is_err() {
                                continue;
                            }
                            // Seeking lands on the preceding key frame, so decode forward until
                            // the frame that covers `t` (or the last frame of the file).
                            let mut best = None;
                            while let Ok(Some(f)) = reader.read_frame() {
                                let reached = f.time + half_frame >= t;
                                best = Some(f);
                                if reached {
                                    break;
                                }
                            }
                            if let Some(f) = best {
                                let _ = ftx.send(to_preview(f, false, g));
                                ctx.request_repaint();
                            }
                        }
                        Request::Play(t, g) => {
                            if reader.seek(t).is_err() {
                                continue;
                            }
                            // Frame times are scheduled relative to this wall-clock start.
                            let wall = Instant::now();
                            loop {
                                // Any new request interrupts playback; keep it for the outer loop.
                                match rx.try_recv() {
                                    Ok(r) => {
                                        pending = Some(r);
                                        break;
                                    }
                                    Err(TryRecvError::Disconnected) => return,
                                    Err(TryRecvError::Empty) => {}
                                }
                                match reader.read_frame() {
                                    Ok(Some(f)) => {
                                        // Skip frames between the key frame and the start point.
                                        if f.time + half_frame < t {
                                            continue;
                                        }
                                        // Wait until the frame is due in real time.
                                        let due = Duration::from_secs_f64((f.time - t).max(0.0));
                                        match due.checked_sub(wall.elapsed()) {
                                            Some(wait) => std::thread::sleep(wait),
                                            // Running behind (slow PC): drop frames that are more
                                            // than a frame late so playback keeps real speed.
                                            None if wall.elapsed() > due + frame_time => continue,
                                            None => {}
                                        }
                                        // The bounded channel blocks here when the UI is slow;
                                        // a closed channel means the editor is gone.
                                        if ftx.send(to_preview(f, true, g)).is_err() {
                                            return;
                                        }
                                        ctx.request_repaint();
                                    }
                                    // End of file or a decoding error: report the end.
                                    _ => {
                                        let _ = ftx.send(eof(g));
                                        ctx.request_repaint();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            })
            .expect("spawn preview thread");

        Self {
            tx,
            frames: frx,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the generation of the newest request; frames from older ones are stale.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Increments the generation counter and returns the new value.
    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Requests the single frame at `t` seconds (used while paused and when scrubbing).
    pub fn seek(&self, t: f64) {
        let g = self.next_generation();
        let _ = self.tx.send(Request::Seek(t, g));
    }

    /// Starts real-time playback from `t` seconds, replacing any playback in progress.
    pub fn play(&self, t: f64) {
        let g = self.next_generation();
        let _ = self.tx.send(Request::Play(t, g));
    }

    /// Stops playback. Bumping the generation makes frames already in flight stale.
    pub fn stop(&self) {
        self.next_generation();
        let _ = self.tx.send(Request::Stop);
    }
}

impl Drop for Preview {
    /// Stops playback; the thread then exits when it sees the request channel close.
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
    }
}
