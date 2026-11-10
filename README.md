# Softromeda Screen Recorder

A screen recorder and video editor for **Windows 10 and 11**, written in Rust: pick what to record from a small capture panel,
record with a floating control bar, then trim, cut, zoom and annotate the result in the built-in
video editor.

It uses only components built into Windows: **no ffmpeg, nothing to install**.

| Part | Windows component |
|---|---|
| Screen capture | DXGI Desktop Duplication (GPU), with a GDI fallback |
| Video encoding / decoding | Media Foundation H.264 (hardware encoder on NVIDIA / Intel / AMD GPUs when available) |
| Audio encoding | Media Foundation AAC and MP3 |
| Microphone / speakers | WASAPI (speakers via loopback) |
| Webcam | Media Foundation camera capture |

## Features

### Recording
| | |
|---|---|
| **Capture modes** | Window / Object (a clicked window is followed when it moves and recorded even when covered; drag for an area), Rectangular Region, Fixed-size Region, Full Screen (any monitor), Repeat Last Region, audio-only |
| **Audio** | Microphone, speakers (system audio), or both mixed; per-source volume; auto-boost when recording speakers; live level meters; record **one program's sound only** (Windows 10 2004+; e.g. just the browser or game); microphone **noise reduction** (fans, hum, hiss; gentle, medium or strong) and **volume levelling** (even speech level, no clipping), applied when saving |
| **Webcam** | Picture-in-picture webcam overlay (corner and size configurable) |
| **Mouse** | Cursor drawn into the video with **smoothed movement** (optional), cursor highlight, **spotlight** (dims everything except a soft circle around the mouse; size and darkness adjustable), left/right click ripples (custom colours), optional **click sound** mixed into the audio |
| **Keyboard** | Optional on-screen keystroke display (`Ctrl + Shift + S`, typed text, …) |
| **Control bar** | Floating bar with timer, pause/resume, save, discard (with confirmation) and audio indicators; colour and size configurable; never captured in the video |
| **Draw on screen** | Pen, highlighter, arrow, rectangle and ellipse in six colours, drawn live over the recorded area (`Ctrl+F10` or **✏ Draw** on the control bar; keys `P H A R E`, `1`–`6`, `Ctrl+Z`, `Delete`; right-click or `Esc` to stop) |
| **Region border** | Red border around the recorded area (amber while paused) |
| **Countdown** | 0–10 s countdown before recording starts |
| **Notification area** | Tray icon with Show / Start recording / Pause / Stop and save / Recordings / Options / Exit; optional close-to-tray and start with Windows (in the tray) |
| **Clean desktop** | Optionally hide the desktop icons from the countdown until the recording ends |
| **Markers** | Drop a marker with `Ctrl+F9` or the 🚩 button on the control bar (e.g. "redo this part"); markers show on the editor timeline |
| **Global hotkeys** | `Ctrl+F11` start / pause / resume, `Ctrl+F12` stop & save, `Ctrl+Shift+F12` discard, `Ctrl+F10` draw, `Ctrl+F9` marker (all configurable) |
| **Timer** | Scheduled recordings with start / end time, optional sleep after the recording completes |
| **Auto stop** | Stop automatically after N minutes |
| **Output** | MP4 (H.264 + AAC), animated GIF, MP3 (audio only); frame rate (up to 60 fps), quality and output scale |
| **Recordings library** | All recordings with thumbnails, length, date and size; search, play, edit, rename, show in folder, delete to the Recycle Bin |
| **Crash recovery** | Recordings are written as fragmented MP4, so a recording interrupted by a crash or power loss can be recovered on the next start |

### Updates
The program looks for a newer release on GitHub at most once a day (can be turned off on the About
page, which also has a *Check for updates* button) and offers to download it, skip it or ask later.

### Video editor
* Frame-accurate preview, silent playback, keyboard navigation (Space, ←/→, Home/End)
* **Trim** by dragging the yellow handles at either end of the timeline
* **Cut** unwanted sections (select a range on the ruler → *Cut selection*)
* **Zoom & pan** effects: zoom into any rectangle for a time range, easing smoothly in and out
* **Speed up a section**: select a slow part and play it 2×, 4× or 8× faster (muted, with a
  fast-forward badge in the export)
* **Markers** dropped while recording show as flags on the timeline and in a list to jump to
* **Remove pauses**: finds quiet stretches in the narration (shortest pause and the margin kept
  next to speech are adjustable) and cuts them in one click; the cuts show on the timeline and can
  be undone
* **Zoom on clicks**: mouse clicks are recorded with the video. Press *✨ Auto zoom* in the editor to
  add zooms around them (clicks close together share one zoom), then adjust or delete them. To add
  them every time a new recording opens, turn on *Options → Mouse & Keyboard → Zoom in on clicks
  automatically*.
* **Annotations** (shown only for the time range you choose): text, callouts, arrows, rectangles,
  ellipses, highlights, blur (for hiding sensitive information) and numbered steps
* **Title pages** inserted anywhere in the clip
* Speed (0.25×–4×, pitch preserved), volume, mute, fade in / fade out
* Undo / redo; edits are auto-saved next to the video (`video.mp4.srproj.json`) so annotations stay re-editable
* **Logo watermark**: a PNG or JPEG logo in a chosen corner of the exported video, with size and
  opacity, shown on the preview
* **Background music**: looped under the whole video with fades, lowered automatically while
  someone speaks (can be turned off), with its own volume
* Export to MP4 or animated GIF, in the original shape or **vertical 9:16** (Shorts, TikTok,
  Reels) / **square 1:1**: the narrower frame follows the mouse pointer smoothly (mouse positions
  are recorded with the video) and is shown on the preview

### Tools
* **Convert to MP4** – convert or resize videos (2160p … 360p) with a quality setting
* **Convert to GIF** – frame rate and width options
* **Extract Audio** – to M4A (AAC) or MP3
* **Join Videos** – concatenate clips of different sizes

### Languages
English, Oʻzbekcha (Uzbek), Русский (Russian), Español (Spanish), Français (French),
Português (Portuguese), 한국어 (Korean), العربية (Arabic), 中文 (Mandarin Chinese,
Simplified), हिन्दी (Hindi), বাংলা (Bengali), Bahasa Indonesia and اردو (Urdu).

The program starts in the Windows display language when it is one of these
(otherwise English). Change it with the 🌐 button in the main window or in
**Options → General → Appearance**.

Translations live in `locales/<code>.tsv` (one `English<TAB>translation` line
per string). `cargo test` fails if any language is missing a string.

## Requirements

Windows 10 or 11 (64-bit). On **Windows N / KN** editions, install the *Media Feature Pack*
(Settings → Apps → Optional features), which provides Media Foundation — the same requirement
FastStone has.

## Building from source

Install Rust (<https://rustup.rs>) with the MSVC toolchain, then:

```bash
cargo build --release
```

The program is `target\release\softromeda-screen-recorder.exe` — a single file; copy it anywhere.

### Installer

`installer\SoftromedaScreenRecorder.iss` is an [Inno Setup](https://jrsoftware.org/isinfo.php) script (version 6.3 or newer, including 7).
With Inno Setup installed, run

```bash
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

to build the program and `installer\output\SoftromedaScreenRecorder-Setup-<version>-x64.exe`. The
installer installs for all users into Program Files (it asks for admin rights), adds Start menu
and optional desktop shortcuts, closes a running copy when updating, and on uninstall asks
whether to remove settings and logs. CI builds the installer on every push.

#### "Running scripts is disabled on this system"

Windows blocks PowerShell scripts by default. There are two ways around it.

**Option 1: allow scripts permanently for your user (recommended).** Run this once in
PowerShell. You don't need admin rights:

```powershell
Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
```

Type `Y` and press Enter. After that, you can run the script directly:

```powershell
.\installer\build.ps1
```

**Option 2: run the script once without changing any settings:**

```powershell
powershell -ExecutionPolicy Bypass -File .\installer\build.ps1
```

## Usage

```bash
softromeda-screen-recorder                 # open the capture panel
softromeda-screen-recorder video.mp4       # open a video in the editor
softromeda-screen-recorder --list-devices  # list monitors, microphones, speakers and webcams
softromeda-screen-recorder --selftest      # check screen capture, encoding and decoding on this PC
```

Headless recording (no GUI) is also available, e.g. for scripts:

```bash
softromeda-screen-recorder --record --duration 30 --region 0,0,1280,720 --mic --speakers --output demo.mp4
```

Run `softromeda-screen-recorder --help` for all options.

### Troubleshooting
* *Options → General → Open log file* opens `softromeda-screen-recorder.log`, which records the capture method,
  the encoder and how many frames were new, e.g.
  `video: 600 frame slots at 60 fps, 598 new screen images (100% fresh)`.
* `softromeda-screen-recorder --selftest` reports which parts work, including 1080p encoding speed.
* `SOFTROMEDA_CAPTURE_METHOD=poll` forces the compatible (GDI) capture method;
  `SOFTROMEDA_ENCODER=software` disables the hardware encoder.

## Project layout

| File | Purpose |
|---|---|
| `src/app.rs` | Capture panel, control bar, options, timer, recovery |
| `src/selector.rs` | Window / region / fixed-region selection overlay |
| `src/recorder.rs` | Capture → pacer → encoder pipeline, cursor / click / keystroke effects, webcam overlay |
| `src/media/capture.rs` | DXGI Desktop Duplication |
| `src/media/mf.rs` | Media Foundation encoding, decoding, remuxing, webcam |
| `src/media/convert.rs`, `audio.rs`, `gifenc.rs` | Pixel conversion, audio mixing / resampling / time-stretch, GIF writer |
| `src/audio.rs` | Microphone and loopback capture (WASAPI via cpal) |
| `src/compose.rs` | Producing the final file from a recording |
| `src/editor/` | Video editor (project model, preview player, render engine) |
| `src/tools.rs` | Conversion tools |
| `src/i18n.rs`, `locales/` | Interface languages |
| `installer/` | Inno Setup installer |

## License

Copyright © 2026 Softromeda. Softromeda Screen Recorder is free and open source software, licensed
under the [GNU General Public License v3.0 or later](LICENSE) with additional terms under section 7,
listed in [NOTICE](NOTICE). In short:

* You may use, study, change and share the program, including for commercial purposes.
* If you distribute it or a program based on it, you must release the complete source code under
  the same license.
* You must keep the copyright notices and the attribution *"Originally created by Softromeda -
  https://softromeda.com/screen-recorder"*, in the source and on the program's About screen.
* Modified versions must be marked as changed and must use a name other than "Softromeda Screen
  Recorder"; the Softromeda name, logos and icon are
  not licensed for use as trademarks.

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). To report a security vulnerability, follow the
[Security Policy](SECURITY.md).
