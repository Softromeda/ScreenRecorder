// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! A pausable wall clock shared between the video and audio capture threads.
//!
//! [`ActiveClock`] measures how long a recording has actually been running, leaving out the time
//! spent paused. The recorder creates one clock per recording and hands clones of it to the
//! screen-capture loop and to every audio capture thread, so that video frames and audio samples
//! are time-stamped against the same timeline and stay in sync across pauses.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Mutable clock state, protected by the mutex in [`ActiveClock`].
#[derive(Debug)]
struct Inner {
    /// The instant the current running stretch began; `None` while stopped or paused.
    running_since: Option<Instant>,
    /// Total running time of all completed stretches (everything before the last pause).
    accumulated: Duration,
    /// Whether the clock has ever been started; distinguishes "paused" from "not yet started".
    started: bool,
}

/// Measures "active" recording time, i.e. wall-clock time excluding pauses.
///
/// The handle is cheap to clone; all clones share the same state, and every method may be called
/// from any thread.
#[derive(Clone, Debug)]
pub struct ActiveClock {
    /// The state shared by all clones of this clock.
    inner: Arc<Mutex<Inner>>,
}

impl Default for ActiveClock {
    /// Creates a clock that has not been started (same as [`ActiveClock::new`]).
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveClock {
    /// Creates a stopped clock with zero elapsed time.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                running_since: None,
                accumulated: Duration::ZERO,
                started: false,
            })),
        }
    }

    /// Starts (or resumes) counting time. Does nothing if the clock is already running.
    pub fn start(&self) {
        let mut i = self.inner.lock().unwrap();
        if i.running_since.is_none() {
            i.running_since = Some(Instant::now());
            i.started = true;
        }
    }

    /// Stops counting time, folding the current running stretch into the accumulated total.
    /// Does nothing if the clock is not running.
    pub fn pause(&self) {
        let mut i = self.inner.lock().unwrap();
        if let Some(since) = i.running_since.take() {
            i.accumulated += since.elapsed();
        }
    }

    /// Continues counting after [`pause`](Self::pause); equivalent to [`start`](Self::start).
    pub fn resume(&self) {
        self.start();
    }

    /// Returns `true` while the clock is counting (started and not paused).
    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().running_since.is_some()
    }

    /// Returns `true` if the clock has been started at least once and is currently paused.
    /// A clock that was never started is not considered paused.
    pub fn is_paused(&self) -> bool {
        let i = self.inner.lock().unwrap();
        i.started && i.running_since.is_none()
    }

    /// Returns the total active time: all completed stretches plus the current one, if running.
    pub fn elapsed(&self) -> Duration {
        let i = self.inner.lock().unwrap();
        i.accumulated + i.running_since.map(|s| s.elapsed()).unwrap_or_default()
    }

    /// Returns [`elapsed`](Self::elapsed) in seconds, as used for frame and sample time stamps.
    pub fn elapsed_secs(&self) -> f64 {
        self.elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Time spent paused is not counted, and counting continues after resuming.
    #[test]
    fn pause_excludes_time() {
        let c = ActiveClock::new();
        assert!(!c.is_paused());
        c.start();
        std::thread::sleep(Duration::from_millis(30));
        c.pause();
        assert!(c.is_paused());
        let a = c.elapsed();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(a, c.elapsed());
        c.resume();
        std::thread::sleep(Duration::from_millis(10));
        assert!(c.elapsed() > a);
    }
}
