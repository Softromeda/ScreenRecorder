// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Capture of one program's sound with WASAPI process loopback (Windows 10 version 2004+).
//!
//! [`AppCapture::start`] activates the virtual "process loopback" audio device for a target
//! process and its child processes (browsers play sound from child processes), then delivers
//! the audio as interleaved 48 kHz stereo `f32` to a callback on a capture thread. The
//! [`programs`] and [`find_program`] helpers list programs with windows and find a running
//! program's process ID by its executable name, so a choice can be stored in the settings.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, PWSTR, Ref, implement};

/// Sample rate of the delivered audio (Windows converts the program's mix to it).
pub const RATE: u32 = 48_000;

/// A running capture of one program's sound. Stops when dropped.
pub struct AppCapture {
    /// Set to ask the capture thread to finish its current wait and exit.
    stop: Arc<AtomicBool>,
    /// The capture thread; taken and joined on drop.
    thread: Option<JoinHandle<()>>,
}

impl AppCapture {
    /// Starts capturing the sound of process `pid` and its child processes.
    ///
    /// `on_audio` receives interleaved stereo `f32` samples at [`RATE`] Hz on the capture
    /// thread, in buffers of about 10 ms. Returns once capture has started, or with the reason
    /// it could not (for example, Windows older than version 2004).
    pub fn start(pid: u32, mut on_audio: impl FnMut(&[f32]) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("program-audio".into())
            .spawn(move || {
                // Set up on this thread (the COM objects stay on it), report the outcome, then
                // pump audio until asked to stop.
                let client = match open(pid) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = pump(&client, &st, &mut on_audio) {
                    log::warn!("program audio capture stopped: {e:#}");
                }
            })?;
        // If set-up hangs, raise the stop flag so the thread exits as soon as it gets going;
        // the handle is dropped without joining so the caller is not blocked.
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                bail!("the program's sound did not become available in time")
            }
        }
    }
}

impl Drop for AppCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The audio client and its event, set up for capture.
struct Client {
    /// The started process-loopback audio client.
    audio: IAudioClient,
    /// Service interface used to read captured packets from `audio`.
    capture: IAudioCaptureClient,
    /// Auto-reset event that WASAPI signals whenever a buffer of audio is ready.
    event: windows::Win32::Foundation::HANDLE,
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: stopping a started client and closing our own event handle.
        unsafe {
            let _ = self.audio.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Completion handler for the asynchronous activation: signals a channel when Windows has
/// finished activating the loopback device.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// Activates the process loopback device for `pid` and starts a 48 kHz stereo capture.
fn open(pid: u32) -> Result<Client> {
    // SAFETY: plain COM initialisation for this thread (already initialised is fine).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Activation parameters: loopback of the target process and everything it starts.
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The parameters travel as a VT_BLOB PROPVARIANT that points at `params`. It must not be
    // cleared (that would free memory we do not own), hence ManuallyDrop.
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: writing the active union member of a default PROPVARIANT; `params` outlives the
    // activation call below, which copies the blob.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }

    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    // SAFETY: valid device path, IID and parameter pointer; the handler is a COM object.
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv as *const PROPVARIANT),
            &handler,
        )
    }
    .context("recording one program's sound needs Windows 10 version 2004 or newer")?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("Windows did not activate the program's sound"))?;

    // SAFETY: COM calls on interfaces we own; out-parameters are local.
    unsafe {
        let mut hr = windows::core::HRESULT(0);
        let mut unknown = None;
        op.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok().context("the program's sound cannot be recorded")?;
        let audio: IAudioClient = unknown.ok_or_else(|| anyhow!("no audio client"))?.cast()?;

        // Process loopback has no mix format of its own: ask for 48 kHz 16-bit stereo and let
        // Windows convert.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        audio
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                2_000_000, // 200 ms buffer, in 100 ns units
                0,
                &format,
                None,
            )
            .context("setting up the program's sound")?;
        let event = CreateEventW(None, false, false, None)?;
        audio.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = audio.GetService()?;
        audio.Start()?;
        Ok(Client {
            audio,
            capture,
            event,
        })
    }
}

/// Reads captured packets until `stop` is set, converting them to `f32` for `on_audio`.
fn pump(client: &Client, stop: &AtomicBool, on_audio: &mut dyn FnMut(&[f32])) -> Result<()> {
    let mut buf: Vec<f32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: waiting on our own event handle.
        if unsafe { WaitForSingleObject(client.event, 100) } != WAIT_OBJECT_0 {
            continue; // timeout: check the stop flag again
        }
        // Drain every packet that is ready.
        loop {
            // SAFETY: GetBuffer/ReleaseBuffer pairs on a started capture client; the buffer is
            // read only between them and its length is `frames` × 4 bytes (16-bit stereo).
            unsafe {
                if client.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                client
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                buf.clear();
                // A silent packet's data must be ignored; emit zeros so timing stays intact.
                // Otherwise scale 16-bit PCM to -1.0..1.0.
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    buf.resize(frames as usize * 2, 0.0);
                } else {
                    let samples =
                        std::slice::from_raw_parts(data as *const i16, frames as usize * 2);
                    buf.extend(samples.iter().map(|s| *s as f32 / 32768.0));
                }
                client.capture.ReleaseBuffer(frames)?;
            }
            on_audio(&buf);
        }
    }
    Ok(())
}

/// The executable file name (such as `chrome.exe`) of process `pid`.
pub fn exe_name(pid: u32) -> Option<String> {
    // SAFETY: querying a process we only open for limited information, then closing it.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}

/// Programs that have a window: (executable name, a window title), one entry per program,
/// sorted by name. Used to let the user choose whose sound to record.
pub fn programs() -> Vec<(String, String)> {
    let own = std::process::id();
    let mut list: Vec<(String, String)> = vec![];
    for w in xcap::Window::all().unwrap_or_default() {
        let (Ok(pid), Ok(title)) = (w.pid(), w.title()) else {
            continue;
        };
        if pid == own || title.trim().is_empty() {
            continue;
        }
        let Some(exe) = exe_name(pid) else { continue };
        if !list.iter().any(|(e, _)| e.eq_ignore_ascii_case(&exe)) {
            list.push((exe, title));
        }
    }
    list.sort_by_key(|(e, _)| e.to_ascii_lowercase());
    list
}

/// The process ID of a running program with a window, found by executable name.
pub fn find_program(exe: &str) -> Option<u32> {
    xcap::Window::all()
        .unwrap_or_default()
        .iter()
        .filter_map(|w| w.pid().ok())
        .find(|pid| exe_name(*pid).is_some_and(|e| e.eq_ignore_ascii_case(exe)))
}
