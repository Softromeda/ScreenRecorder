// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Screen capture with DXGI Desktop Duplication, the GPU-based API used by Windows' own
//! recorders.
//!
//! [`Duplicator`] attaches to one monitor and, on each [`Duplicator::grab`], copies only the
//! recorded region from the GPU into a CPU-readable staging texture, returning tightly packed BGRA
//! pixels (the byte order the encoder path expects). The recorder's capture thread uses it for
//! area and full-screen recordings and falls back to GDI polling when it is unavailable.

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// Result of one [`Duplicator::grab`] call.
pub enum Grab {
    /// The desktop image changed: new BGRA pixels, region-sized and tightly packed
    /// (`width * 4` bytes per row).
    Frame(Vec<u8>),
    /// No new desktop image within the timeout (or only the pointer moved, or duplication was
    /// restarted); the caller repeats its previous frame.
    Unchanged,
}

/// A Desktop Duplication session for one region of one monitor.
///
/// The D3D11 device is created on the adapter that drives the monitor. All D3D objects are used
/// only from the capture thread that creates them.
pub struct Duplicator {
    /// D3D11 device on the adapter that drives the monitor; also needed to recreate the
    /// duplication after access is lost.
    device: ID3D11Device,
    /// Immediate context used for the region copy and for mapping the staging texture.
    context: ID3D11DeviceContext,
    /// The monitor's DXGI output, kept to restart duplication.
    output: IDXGIOutput1,
    /// The active duplication session that hands out desktop images.
    duplication: IDXGIOutputDuplication,
    /// CPU-readable texture the size of the region; frames are copied into it and mapped.
    staging: ID3D11Texture2D,
    /// Captured area as (x, y, width, height) in pixels relative to the monitor's top-left corner.
    region: (u32, u32, u32, u32),
}

impl Duplicator {
    /// Starts duplicating the monitor whose top-left desktop coordinate is `monitor_origin` and
    /// prepares to copy `region` (x, y, width, height in physical pixels relative to that monitor).
    ///
    /// Fails when no output matches the origin, when the display is rotated (the caller then uses
    /// the GDI method), or when desktop duplication is unavailable (e.g. remote sessions or too
    /// many duplication clients). The region must lie within the monitor.
    pub fn new(monitor_origin: (i32, i32), region: (u32, u32, u32, u32)) -> Result<Self> {
        // SAFETY: plain COM/D3D calls on objects created here; out-pointers refer to locals.
        unsafe {
            // Find the adapter output whose desktop rectangle starts at the monitor's origin.
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("DXGI unavailable")?;
            let mut found = None;
            'adapters: for a in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(a) else {
                    break;
                };
                for o in 0.. {
                    let Ok(output) = adapter.EnumOutputs(o) else {
                        break;
                    };
                    let desc = output.GetDesc()?;
                    let r = desc.DesktopCoordinates;
                    if (r.left, r.top) == monitor_origin {
                        // Duplicated images of rotated displays are not rotated, so the region
                        // would not match; leave those to the compatible method.
                        if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                            && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                        {
                            bail!("rotated displays are captured with the compatible method");
                        }
                        found = Some((adapter, output));
                        break 'adapters;
                    }
                }
            }
            let (adapter, output) =
                found.ok_or_else(|| anyhow!("monitor not found for DXGI capture"))?;

            // The device must live on the GPU that drives this monitor.
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter.cast::<IDXGIAdapter>()?,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("creating a Direct3D device")?;
            let device = device.ok_or_else(|| anyhow!("no D3D device"))?;
            let context = context.ok_or_else(|| anyhow!("no D3D context"))?;
            let output: IDXGIOutput1 = output.cast()?;
            let duplication = output
                .DuplicateOutput(&device)
                .context("desktop duplication is not available")?;

            // A staging texture of the region size, which the CPU can map and read.
            let (_, _, w, h) = region;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
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
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            Ok(Self {
                device,
                context,
                output,
                duplication,
                staging: staging.ok_or_else(|| anyhow!("no staging texture"))?,
                region,
            })
        }
    }

    /// Waits up to `timeout_ms` milliseconds for a new desktop image and returns the region's
    /// pixels, or [`Grab::Unchanged`] when there is none.
    ///
    /// When access to the duplication is lost (display mode change, UAC secure desktop,
    /// exclusive full-screen application), the duplication is recreated and `Unchanged` is
    /// returned. Other errors are passed to the caller.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        // SAFETY: the duplication and staging objects belong to this thread; every acquired frame
        // is released before returning.
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Unchanged),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // Display mode change, UAC prompt, full-screen application: the old
                    // duplication is unusable, so start a new one.
                    self.duplication = self
                        .output
                        .DuplicateOutput(&self.device)
                        .context("restarting desktop duplication")?;
                    return Ok(Grab::Unchanged);
                }
                Err(e) => return Err(e).context("capturing the screen"),
            }
            // A zero present time means only the pointer changed, not the desktop image.
            if info.LastPresentTime == 0 {
                let _ = self.duplication.ReleaseFrame();
                return Ok(Grab::Unchanged);
            }
            let result = self.copy_region(resource);
            let _ = self.duplication.ReleaseFrame();
            result.map(Grab::Frame)
        }
    }

    /// Copies the region out of an acquired desktop texture and reads it back as packed BGRA.
    ///
    /// Fails for non-BGRA desktop formats (e.g. HDR displays).
    ///
    /// # Safety
    /// Must be called on the capture thread while the frame that `resource` belongs to is still
    /// acquired (before `ReleaseFrame`).
    unsafe fn copy_region(&self, resource: Option<IDXGIResource>) -> Result<Vec<u8>> {
        // SAFETY: the caller guarantees the frame is still acquired. The mapped pointer is read
        // only between `Map` and `Unmap`, each row read is `w * 4` bytes within `RowPitch`, and
        // `out` is sized for `h` packed rows.
        unsafe {
            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| anyhow!("no desktop image"))?
                .cast()?;
            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut src_desc);
            if src_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                bail!("unsupported desktop format {:?} (HDR?)", src_desc.Format);
            }
            // GPU copy of just the region into the staging texture, then map it for reading.
            let (x, y, w, h) = self.region;
            let bx = D3D11_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + w,
                bottom: y + h,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &texture, 0, Some(&bx));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("reading the screen image")?;
            // Rows in the mapped texture are `RowPitch` bytes apart (padded), so copy row by row
            // into a tightly packed buffer.
            let row = w as usize * 4;
            let mut out = vec![0u8; row * h as usize];
            for line in 0..h as usize {
                let src = (mapped.pData as *const u8).add(line * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(line * row), row);
            }
            self.context.Unmap(&self.staging, 0);
            Ok(out)
        }
    }
}
