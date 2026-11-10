// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Background jobs with progress reporting (saving, exporting, converting).
//!
//! Long-running work such as saving a recording, exporting from the editor or converting files in
//! the tools window runs on a worker thread started by [`Job::spawn`]. The worker reports progress
//! through its [`Job`] handle, and the UI polls [`Job::status`] each frame to draw a progress bar
//! and to react when the job finishes or fails.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;

/// The current state of a background job.
#[derive(Debug, Clone)]
pub enum JobStatus {
    /// The job is still working.
    Running {
        /// Fraction of the work done, from 0.0 to 1.0.
        progress: f32,
        /// Short description of the current step, shown next to the progress bar.
        message: String,
    },
    /// The job completed successfully.
    Done {
        /// The file the job wrote.
        output: PathBuf,
    },
    /// The job failed; the string is the full error chain, ready to show to the user.
    Failed(String),
}

/// A shared handle to a background job's status.
///
/// Clones share the same status, so the worker thread and the UI can each hold one.
#[derive(Clone)]
pub struct Job {
    /// Short human-readable name of the job (for example "Export").
    #[allow(dead_code)]
    pub title: String,
    /// The status shared between the worker thread and every clone of the handle.
    status: Arc<Mutex<JobStatus>>,
}

impl Job {
    /// Creates a job in the [`JobStatus::Running`] state with zero progress.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: Arc::new(Mutex::new(JobStatus::Running {
                progress: 0.0,
                message: "Starting…".into(),
            })),
        }
    }

    /// Returns a snapshot of the current status.
    pub fn status(&self) -> JobStatus {
        self.status.lock().unwrap().clone()
    }

    /// Replaces the current status.
    pub fn set(&self, s: JobStatus) {
        *self.status.lock().unwrap() = s;
    }

    /// Reports progress from the worker; `progress` is clamped to the range 0.0–1.0.
    pub fn set_progress(&self, progress: f32, message: impl Into<String>) {
        self.set(JobStatus::Running {
            progress: progress.clamp(0.0, 1.0),
            message: message.into(),
        });
    }

    /// Returns `true` once the job has either succeeded or failed.
    pub fn is_finished(&self) -> bool {
        !matches!(self.status(), JobStatus::Running { .. })
    }

    /// Runs `f` on a new background thread and returns a handle to its status.
    ///
    /// `f` receives the job so it can call [`set_progress`](Self::set_progress). When it returns,
    /// the status becomes [`JobStatus::Done`] with the returned output path (the accompanying
    /// message string is ignored) or [`JobStatus::Failed`] with the formatted error chain.
    pub fn spawn<F>(title: impl Into<String>, f: F) -> Self
    where
        F: FnOnce(&Job) -> Result<(PathBuf, String)> + Send + 'static,
    {
        let job = Job::new(title);
        let j = job.clone();
        std::thread::spawn(move || {
            let result = f(&j);
            j.set(match result {
                Ok((output, _message)) => JobStatus::Done { output },
                Err(e) => JobStatus::Failed(format!("{e:#}")),
            });
        });
        job
    }
}
