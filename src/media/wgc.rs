// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Window capture with Windows.Graphics.Capture (Windows 10 version 1903 and newer).
//!
//! [`WindowCapture`] records one window's own content wherever it is moved and even while other
//! windows cover it; the recorder uses it for "Window / Object" recordings and falls back to area
//! capture when it fails. [`window_bounds`] reports where the window currently is, so the
//! region border and overlays can follow it.

use anyhow::{Context, Result, anyhow, bail};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
use windows::core::Interface;

/// Converts a window handle stored as an integer back into an `HWND`.
fn hwnd(handle: isize) -> HWND {
    HWND(handle as *mut core::ffi::c_void)
}

/// Returns the window's visible bounds on the desktop as (x, y, width, height) in physical
/// pixels, excluding the invisible resize borders.
///
/// Returns `None` when the window no longer exists, is minimised, or has an empty frame.
pub fn window_bounds(handle: isize) -> Option<(i32, i32, u32, u32)> {
    let h = hwnd(handle);
    // SAFETY: the calls validate the handle themselves; `r` is a local RECT of the size passed.
    unsafe {
        if !IsWindow(Some(h)).as_bool() || IsIconic(h).as_bool() {
            return None;
        }
        // The DWM extended frame bounds are the visible frame, unlike GetWindowRect.
        let mut r = RECT::default();
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
        let (w, hh) = (r.right - r.left, r.bottom - r.top);
        (w > 0 && hh > 0).then_some((r.left, r.top, w as u32, hh as u32))
    }
}

/// A running Windows.Graphics.Capture session for one window.
///
/// Frames are pulled with [`WindowCapture::grab`]; the session is closed on drop.
pub struct WindowCapture {
    /// Immediate context used to copy frames into the staging texture and map it.
    context: ID3D11DeviceContext,
    /// Hardware D3D11 device that owns the frame pool's textures and the staging texture.
    device: ID3D11Device,
    /// The WinRT wrapper of `device`, needed by the frame pool.
    d3d: IDirect3DDevice,
    /// Free-threaded pool of two frame buffers, recreated when the window is resized.
    pool: Direct3D11CaptureFramePool,
    /// The capture session; closed on drop.
    session: GraphicsCaptureSession,
    /// Content size the frame pool buffers were created for.
    size: SizeInt32,
    /// CPU-readable copy target and its texture width and height, created on first use and
    /// recreated when the frame texture size changes.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl WindowCapture {
    /// Starts capturing the window `handle` (an `HWND` as an integer).
    ///
    /// Initialises WinRT on the calling thread, creates a hardware D3D11 device and a frame pool
    /// sized to the window, and starts the session without the system cursor (the recorder draws
    /// its own) and, where supported, without the yellow capture border. Fails on Windows versions
    /// without Graphics Capture or for windows that cannot be captured.
    pub fn new(handle: isize) -> Result<Self> {
        // SAFETY: plain WinRT initialisation for this thread; takes no pointers.
        unsafe {
            // An error because WinRT is already initialised on this thread is harmless.
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            bail!("window capture needs Windows 10 version 1903 or newer");
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: `CreateForWindow` validates the handle and fails for windows that are gone.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd(handle)) }
            .context("this window cannot be captured")?;

        // A hardware D3D11 device with BGRA support, wrapped as a WinRT IDirect3DDevice for the
        // frame pool.
        let mut device = None;
        let mut context = None;
        // SAFETY: out-parameters are locals that outlive the call.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("creating a Direct3D device")?;
        }
        let device = device.ok_or_else(|| anyhow!("no D3D device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D context"))?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: `dxgi` is a valid DXGI device obtained from `device` just above.
        let d3d: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // The recorder draws its own cursor; the yellow border can be disabled on Windows 11.
        // Both setters fail on older builds, which is acceptable.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Self {
            context,
            device,
            d3d,
            pool,
            session,
            size,
            staging: None,
        })
    }

    /// Returns the newest window image as tightly packed BGRA with its width and height, or
    /// `None` when no new frame arrived since the last call (window unchanged or minimised).
    ///
    /// The returned size follows the window, so it changes when the window is resized; the
    /// caller letterboxes it into the video size.
    pub fn grab(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(None);
        };
        // The frame texture can be larger than the window content (after a resize, until the
        // pool is recreated), so copy only the content area.
        let content = frame.ContentSize()?;
        // SAFETY: the surface of a Direct3D11 capture frame always exposes the DXGI interface
        // access interface, and `frame` keeps the texture alive until it is closed below.
        let tex: ID3D11Texture2D = unsafe {
            frame
                .Surface()?
                .cast::<IDirect3DDxgiInterfaceAccess>()?
                .GetInterface()?
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a local out-parameter of the right type.
        unsafe { tex.GetDesc(&mut desc) };
        let w = (content.Width.max(1) as u32).min(desc.Width);
        let h = (content.Height.max(1) as u32).min(desc.Height);

        // A CPU-readable staging texture matching the frame texture, reused while its size is
        // unchanged.
        if !matches!(&self.staging, Some((_, sw, sh)) if (*sw, *sh) == (desc.Width, desc.Height)) {
            let sdesc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            // SAFETY: `sdesc` is a valid description and `t` a local out-parameter.
            unsafe { self.device.CreateTexture2D(&sdesc, None, Some(&mut t))? };
            self.staging = Some((
                t.ok_or_else(|| anyhow!("no staging texture"))?,
                desc.Width,
                desc.Height,
            ));
        }
        let staging = &self.staging.as_ref().unwrap().0;
        let mut out = vec![0u8; (w * h * 4) as usize];
        // Copy the frame to the staging texture, map it and pack the content rows (the mapped
        // rows are `RowPitch` bytes apart).
        // SAFETY: the staging texture has the same size and format as `tex`, and only `w * 4`
        // bytes of the first `h` rows are read, which lie inside the mapped texture.
        unsafe {
            self.context.CopyResource(staging, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            let row = (w * 4) as usize;
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(y * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(y * row),
                    row,
                );
            }
            self.context.Unmap(staging, 0);
        }
        let _ = frame.Close();

        // The window was resized: recreate the pool buffers at the new size for later frames.
        if content.Width != self.size.Width || content.Height != self.size.Height {
            self.size = content;
            self.pool.Recreate(
                &self.d3d,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content,
            )?;
        }
        Ok(Some((out, w, h)))
    }
}

/// Stops the capture session and releases the frame pool.
impl Drop for WindowCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
