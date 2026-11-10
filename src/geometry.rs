// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Screen and window geometry helpers.
//!
//! Defines [`CaptureRegion`] (the area a recording covers), [`MonitorInfo`] and [`WindowInfo`]
//! snapshots taken from `xcap`, plus helpers to enumerate monitors and windows, clamp a region to
//! a monitor and centre egui windows on the primary display.
//!
//! Capture coordinates are physical pixels as reported by `xcap`; egui window placement uses
//! logical points, and [`MonitorInfo::units_per_point`] converts between the two.

use serde::{Deserialize, Serialize};
use xcap::Monitor;

/// A capture area in physical pixels, relative to the top-left corner of a monitor.
///
/// Saved in the settings so that "Repeat Last Region" can record the same area again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRegion {
    /// `xcap` identifier of the monitor the region lies on.
    pub monitor_id: u32,
    /// Left edge, in pixels from the monitor's left edge.
    pub x: u32,
    /// Top edge, in pixels from the monitor's top edge.
    pub y: u32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Handle (`HWND`) of a window to record and follow when it moves, for "Window/Object" mode.
    ///
    /// Not serialised: a window handle is only valid while that window exists.
    #[serde(skip)]
    pub window: Option<isize>,
}

impl CaptureRegion {
    /// Human-readable size and position, e.g. `1280 × 720 at (0, 0)`, for display in the UI.
    pub fn describe(&self) -> String {
        format!(
            "{} × {} at ({}, {})",
            self.width, self.height, self.x, self.y
        )
    }
}

/// A snapshot of one monitor's identity, position and size.
///
/// Position and size are physical pixels in the virtual-desktop coordinate system (the primary
/// monitor's top-left corner is the origin; other monitors may have negative coordinates).
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// `xcap` monitor identifier, used to find the monitor again later.
    pub id: u32,
    /// Friendly display name (falls back to the device name or "Monitor").
    pub name: String,
    /// Left edge in virtual-desktop pixels.
    pub x: i32,
    /// Top edge in virtual-desktop pixels.
    pub y: i32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// DPI scale factor (1.0 = 96 DPI); never below 0.1.
    pub scale_factor: f32,
    /// Whether this is the Windows primary monitor.
    pub is_primary: bool,
}

impl MonitorInfo {
    /// Builds a snapshot from an `xcap` monitor, or `None` if its id or geometry cannot be read.
    pub fn from_monitor(m: &Monitor) -> Option<Self> {
        Some(Self {
            id: m.id().ok()?,
            name: m
                .friendly_name()
                .or_else(|_| m.name())
                .unwrap_or_else(|_| "Monitor".into()),
            x: m.x().ok()?,
            y: m.y().ok()?,
            width: m.width().ok()?,
            height: m.height().ok()?,
            scale_factor: m.scale_factor().unwrap_or(1.0).max(0.1),
            is_primary: m.is_primary().unwrap_or(false),
        })
    }

    /// A capture region that covers the whole monitor.
    pub fn full_region(&self) -> CaptureRegion {
        CaptureRegion {
            monitor_id: self.id,
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
            window: None,
        }
    }

    /// Whether the global point (`gx`, `gy`), in virtual-desktop pixels, lies on this monitor.
    /// The right and bottom edges are exclusive.
    pub fn contains(&self, gx: i32, gy: i32) -> bool {
        gx >= self.x
            && gy >= self.y
            && gx < self.x + self.width as i32
            && gy < self.y + self.height as i32
    }

    /// Number of physical pixels per logical point (egui point) on this monitor.
    pub fn units_per_point(&self) -> f32 {
        self.scale_factor
    }

    /// The monitor rectangle in logical points, as used for egui window placement.
    pub fn logical_rect(&self) -> egui::Rect {
        let u = self.units_per_point();
        egui::Rect::from_min_size(
            egui::pos2(self.x as f32 / u, self.y as f32 / u),
            egui::vec2(self.width as f32 / u, self.height as f32 / u),
        )
    }
}

/// Lists all monitors that could be queried; returns an empty list if enumeration fails.
pub fn list_monitors() -> Vec<MonitorInfo> {
    Monitor::all()
        .map(|all| all.iter().filter_map(MonitorInfo::from_monitor).collect())
        .unwrap_or_default()
}

/// Finds the live `xcap` monitor with the given id, e.g. to start capturing from it.
pub fn find_monitor(id: u32) -> Option<Monitor> {
    Monitor::all()
        .ok()?
        .into_iter()
        .find(|m| m.id().ok() == Some(id))
}

/// The primary monitor, or the first monitor found if none is marked primary.
pub fn primary_monitor() -> Option<MonitorInfo> {
    let all = list_monitors();
    all.iter()
        .find(|m| m.is_primary)
        .cloned()
        .or_else(|| all.first().cloned())
}

/// Top-left position, in logical points, that centres a window of `size` on the primary monitor.
///
/// The position is clamped so the title bar stays on screen. Returns `None` if no monitor is
/// found. The monitor rectangle is looked up once and cached for the life of the process.
pub fn centered_on_primary(size: egui::Vec2) -> Option<egui::Pos2> {
    // Viewport builders run every frame, so cache the monitor lookup instead of repeating it.
    static PRIMARY: std::sync::OnceLock<Option<egui::Rect>> = std::sync::OnceLock::new();
    let m = (*PRIMARY.get_or_init(|| primary_monitor().map(|m| m.logical_rect())))?;
    Some(center_in(m, size))
}

/// Top-left position that centres a box of `size` inside `monitor`, never left of or above the
/// monitor's top-left corner (so an oversized window keeps its title bar visible).
pub fn center_in(monitor: egui::Rect, size: egui::Vec2) -> egui::Pos2 {
    let x = monitor.center().x - size.x / 2.0;
    let y = monitor.center().y - size.y / 2.0;
    egui::pos2(x.max(monitor.left()), y.max(monitor.top()))
}

/// Adds the application icon and a start position centred on the primary monitor to a viewport
/// builder. `size` is the window's inner size in logical points.
pub fn centered(builder: egui::ViewportBuilder, size: [f32; 2]) -> egui::ViewportBuilder {
    let builder = builder.with_icon(crate::util::app_icon());
    match centered_on_primary(size.into()) {
        Some(pos) => builder.with_position(pos),
        None => builder,
    }
}

/// The monitor containing the global point (`gx`, `gy`) in virtual-desktop pixels.
///
/// Falls back to the primary monitor, then to the first monitor, when the point is off-screen.
pub fn monitor_at(gx: i32, gy: i32) -> Option<MonitorInfo> {
    let all = list_monitors();
    all.iter()
        .find(|m| m.contains(gx, gy))
        .cloned()
        .or_else(|| all.iter().find(|m| m.is_primary).cloned())
        .or_else(|| all.first().cloned())
}

/// Clamps a rectangle given in monitor-local pixels to the bounds of monitor `m`.
///
/// The rectangle may extend past any edge (or have negative coordinates); only the part on the
/// monitor is kept. Returns `None` if that part is narrower or shorter than 8 pixels, which is
/// too small to record.
pub fn clamp_region(m: &MonitorInfo, x: i64, y: i64, w: i64, h: i64) -> Option<CaptureRegion> {
    let x0 = x.clamp(0, m.width as i64);
    let y0 = y.clamp(0, m.height as i64);
    let x1 = (x + w).clamp(0, m.width as i64);
    let y1 = (y + h).clamp(0, m.height as i64);
    let (w, h) = (x1 - x0, y1 - y0);
    if w < 8 || h < 8 {
        return None;
    }
    Some(CaptureRegion {
        monitor_id: m.id,
        x: x0 as u32,
        y: y0 as u32,
        width: w as u32,
        height: h as u32,
        window: None,
    })
}

/// A top-level window that can be picked for "Window/Object" recording.
///
/// Position and size are physical pixels in virtual-desktop coordinates.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// Window handle (`HWND`) as reported by `xcap`.
    pub id: u32,
    /// Window title text.
    pub title: String,
    /// Name of the owning application.
    pub app_name: String,
    /// Left edge in virtual-desktop pixels.
    pub x: i32,
    /// Top edge in virtual-desktop pixels.
    pub y: i32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Z-order reported by `xcap`; higher values are nearer the top.
    pub z: i32,
}

/// Lists visible top-level windows, topmost first.
///
/// Minimised windows, windows whose geometry cannot be read and windows smaller than 16 × 16
/// pixels (typically invisible helper windows) are left out.
pub fn list_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = xcap::Window::all()
        .map(|all| {
            all.iter()
                .filter(|w| !w.is_minimized().unwrap_or(false))
                .filter_map(|w| {
                    Some(WindowInfo {
                        id: w.id().ok()?,
                        title: w.title().unwrap_or_default(),
                        app_name: w.app_name().unwrap_or_default(),
                        x: w.x().ok()?,
                        y: w.y().ok()?,
                        width: w.width().ok()?,
                        height: w.height().ok()?,
                        z: w.z().unwrap_or(0),
                    })
                })
                .filter(|w| w.width >= 16 && w.height >= 16)
                .collect()
        })
        .unwrap_or_default();
    // Topmost first, so hit-testing can take the first window under the pointer.
    out.sort_by_key(|w| std::cmp::Reverse(w.z));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920 × 1080 primary monitor at the origin.
    fn mon() -> MonitorInfo {
        MonitorInfo {
            id: 1,
            name: "m".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.0,
            is_primary: true,
        }
    }

    /// Centres a window on a secondary monitor and keeps an oversized one at the top-left corner.
    #[test]
    fn centering() {
        let m = egui::Rect::from_min_size(egui::pos2(1920.0, 0.0), egui::vec2(1600.0, 900.0));
        assert_eq!(
            center_in(m, egui::vec2(700.0, 250.0)),
            egui::pos2(2370.0, 325.0)
        );
        // Larger than the monitor: keep the top-left corner on screen.
        assert_eq!(
            center_in(m, egui::vec2(2000.0, 1000.0)),
            egui::pos2(1920.0, 0.0)
        );
    }

    /// A rectangle fully inside the monitor is returned unchanged.
    #[test]
    fn clamp_inside() {
        let r = clamp_region(&mon(), 10, 20, 100, 50).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (10, 20, 100, 50));
    }

    /// A rectangle crossing the edges is cut to the monitor; one entirely off it is rejected.
    #[test]
    fn clamp_outside_edges() {
        let r = clamp_region(&mon(), -50, 1000, 200, 200).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 1000, 150, 80));
        assert!(clamp_region(&mon(), 3000, 0, 10, 10).is_none());
    }
}
