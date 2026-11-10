// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Update check: asks GitHub for the latest release of the program and compares its version
//! with the running one.
//!
//! [`latest`] makes one HTTPS request with WinHTTP (the HTTP client built into Windows, which
//! follows the system's proxy settings and certificates). [`is_newer`] compares version
//! numbers such as `1.2.0` and `v1.10.3`. The app runs the check on a background thread at most
//! once a day, or when the user asks for it on the About page.

use serde::Deserialize;

/// GitHub repository the releases come from.
pub const REPOSITORY: &str = "softromeda/screenrecorder";

/// The newest published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Version number without a leading `v` (for example `1.1.0`).
    pub version: String,
    /// Web page of the release, where the installer can be downloaded.
    pub url: String,
}

/// Parses `1.2.3` or `v1.2.3` (anything after a `-` or `+` is ignored) into numbers; missing
/// parts count as 0.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let v = version.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

/// Returns whether `latest` is a higher version than `current`. Unreadable versions never count
/// as newer.
///
/// Versions compare numerically part by part (tuple ordering), so `1.10.0` is newer than
/// `1.9.9`. Pre-release suffixes are dropped, so `1.1.0-beta` equals `1.1.0` and is not offered
/// over it.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The part of GitHub's release JSON that is used.
#[derive(Deserialize)]
struct GithubRelease {
    /// Git tag of the release, usually the version with a leading `v`.
    tag_name: String,
    /// Web page of the release.
    html_url: String,
    /// Whether the release is an unpublished draft.
    #[serde(default)]
    draft: bool,
    /// Whether the release is marked as a pre-release; such releases are not offered.
    #[serde(default)]
    prerelease: bool,
}

/// Fetches the latest published release from GitHub.
///
/// Fails with a readable reason when there is no connection, the repository has no public
/// releases yet, or GitHub answers with an error.
#[cfg(windows)]
pub fn latest() -> anyhow::Result<Release> {
    let path = format!("/repos/{REPOSITORY}/releases/latest");
    let (status, body) = http_get("api.github.com", &path)?;
    match status {
        200 => {}
        404 => anyhow::bail!("no published release was found"),
        s => anyhow::bail!("GitHub answered with status {s}"),
    }
    let r: GithubRelease = serde_json::from_slice(&body)?;
    if r.draft || r.prerelease {
        anyhow::bail!("the latest release is not final");
    }
    Ok(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_string(),
        url: r.html_url,
    })
}

/// Makes an HTTPS GET request with WinHTTP and returns the status code and body.
#[cfg(windows)]
fn http_get(host: &str, path: &str) -> anyhow::Result<(u32, Vec<u8>)> {
    use anyhow::{Context, anyhow};
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::{HSTRING, PCWSTR};

    /// Closes a WinHTTP handle when dropped.
    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle came from WinHTTP and is closed once.
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }
    let open = |h: *mut core::ffi::c_void, what: &str| {
        if h.is_null() {
            Err(anyhow!("{what}: {}", windows::core::Error::from_thread()))
        } else {
            Ok(Handle(h))
        }
    };

    let agent = HSTRING::from(format!("Softromeda-Recorder/{}", env!("CARGO_PKG_VERSION")));
    // SAFETY: WinHTTP calls with valid strings and handles owned by the guards above; buffers
    // passed for reading are sized by the counts given with them.
    unsafe {
        let session = open(
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "starting the HTTP client",
        )?;
        // Give up quickly when offline: 5 s to resolve / connect, 10 s to send / receive.
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000);
        let connect = open(
            WinHttpConnect(
                session.0,
                &HSTRING::from(host),
                INTERNET_DEFAULT_HTTPS_PORT,
                0,
            ),
            "connecting",
        )?;
        let request = open(
            WinHttpOpenRequest(
                connect.0,
                &HSTRING::from("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "opening the request",
        )?;
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n"
            .encode_utf16()
            .collect();
        let _ = WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD);
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).context("no internet connection")?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).context("no answer")?;

        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut len,
            std::ptr::null_mut(),
        )?;

        // Read the body in whatever pieces WinHTTP has ready.
        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available)?;
            if available == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                body[start..].as_mut_ptr() as *mut _,
                available,
                &mut read,
            )?;
            body.truncate(start + read as usize);
            // A release JSON is a few kilobytes; cap at 4 MiB so a misbehaving server cannot
            // exhaust memory.
            if body.len() > 4 << 20 {
                anyhow::bail!("the answer is unexpectedly large");
            }
        }
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Versions compare by number, not as text, and tags may start with `v`.
    #[test]
    fn version_comparison() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("v1.10.0", "1.9.9"));
        assert!(is_newer("2", "1.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("v0.9.0", "1.0.0"));
        assert!(!is_newer("1.1.0-beta", "1.1.0"));
        assert!(!is_newer("nonsense", "1.0.0"));
    }
}
