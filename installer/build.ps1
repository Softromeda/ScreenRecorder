# Softromeda Screen Recorder
# Copyright (C) 2026 Softromeda <https://softromeda.com>
# Originally created by Softromeda - https://softromeda.com/screen-recorder
# SPDX-License-Identifier: GPL-3.0-or-later
#
# This program is free software: you can redistribute it and/or modify it under the terms of
# the GNU General Public License as published by the Free Software Foundation, either version 3
# of the License, or (at your option) any later version, with the additional terms listed in
# the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

# Builds Softromeda Screen Recorder and its installer.
#
# Requirements: Rust (cargo) and Inno Setup 6.3+ or 7 (https://jrsoftware.org/isinfo.php).
# Usage (from any folder):  powershell -ExecutionPolicy Bypass -File installer\build.ps1
# Result: installer\output\SoftromedaScreenRecorder-Setup-<version>-x64.exe

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Version from Cargo.toml ([package] version = "x.y.z").
$version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Softromeda Screen Recorder $version"

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

# Find the Inno Setup compiler.
$iscc = (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    $candidates = @()
    # Install location recorded by the Inno Setup installer (newest version first).
    foreach ($ver in 7, 6) {
        foreach ($hive in 'HKLM:\Software\Microsoft', 'HKLM:\Software\WOW6432Node\Microsoft', 'HKCU:\Software\Microsoft') {
            $key = Get-ItemProperty "$hive\Windows\CurrentVersion\Uninstall\Inno Setup ${ver}_is1" -ErrorAction SilentlyContinue
            if ($key -and $key.InstallLocation) { $candidates += Join-Path $key.InstallLocation 'ISCC.exe' }
        }
        $candidates += "$env:ProgramFiles\Inno Setup $ver\ISCC.exe",
            "${env:ProgramFiles(x86)}\Inno Setup $ver\ISCC.exe",
            "$env:LOCALAPPDATA\Programs\Inno Setup $ver\ISCC.exe"
    }
    $iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) { throw 'Inno Setup (ISCC.exe) not found. Install Inno Setup 6 or 7 from https://jrsoftware.org/isdl.php' }
Write-Host "Using $iscc"

& $iscc "/DAppVersion=$version" 'installer\SoftromedaScreenRecorder.iss'
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed' }
Get-ChildItem 'installer\output\*.exe' | ForEach-Object { Write-Host "Installer: $($_.FullName)" }
