// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Build script: embeds the program icon and version information into the Windows executable.
//!
//! On Windows targets it uses `winresource` to compile a resource file containing
//! `assets/icon.ico` and the product, company and copyright strings that Explorer shows in the
//! file's properties. On other targets it does nothing.

/// Compiles and links the Windows resources; skipped when the target OS is not Windows.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    // Check the target OS rather than the host, so cross-compiling for Windows embeds the icon.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "Softromeda Screen Recorder")
        .set(
            "FileDescription",
            "Softromeda Screen Recorder - screen recorder and video editor",
        )
        .set("CompanyName", "Softromeda")
        .set("LegalCopyright", "Copyright (c) 2026 Softromeda");
    // Missing resource tools (Windows SDK `rc.exe` or MinGW `windres`) must not break the
    // build: the program still works, it just has no icon in Explorer.
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the program icon: {e}");
    }
}
