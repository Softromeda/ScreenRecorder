// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Command-line modes that run without the GUI.
//!
//! * [`record`] – headless recording (`softromeda-screen-recorder --record ...`) using the same
//!   [`Recorder`] and [`compose`] pipeline as the GUI, with the saved settings as defaults.
//! * [`list_devices`] – `--list-devices`: monitors, audio devices and webcams.
//! * [`selftest`] – `--selftest`: checks encoding, decoding and screen capture on this PC.
//!
//! `main` dispatches to these before any window is created; results go to stdout and progress
//! and warnings to stderr.

use std::io::BufRead;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::compose;
use crate::geometry;
use crate::recorder::Recorder;
use crate::settings::{OutputFormat, Settings};

/// The argument following option `name` in `args` (e.g. `"30"` for `--duration 30`), or `None`
/// if the option is absent or last.
fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// Records the screen without the GUI, then saves the file and prints its path to stdout.
///
/// Options (all optional): `--format mp4|gif|mp3`, `--fps N`, `--mic`, `--speakers`,
/// `--webcam`, `--monitor I` (index as printed by `--list-devices`, default the primary monitor),
/// `--region X,Y,W,H` (monitor-local pixels, default the whole monitor), `--duration SECONDS`
/// (otherwise recording stops when Enter is pressed) and `--output PATH`. Other options come from
/// the saved settings; audio and webcam are off unless their flags are given. Fails on invalid
/// options, if recording cannot start, or if the video thread fails while recording.
pub fn record(args: &[String]) -> Result<()> {
    // Start from the saved settings and override them with the command-line options.
    let mut settings = Settings::load();
    if let Some(f) = value(args, "--format") {
        settings.format = OutputFormat::ALL
            .into_iter()
            .find(|x| x.extension() == f.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("unknown format '{f}'"))?;
    }
    if let Some(fps) = value(args, "--fps") {
        settings.fps = fps.parse().context("--fps")?;
    }
    settings.record_microphone = args.iter().any(|a| a == "--mic");
    settings.record_speakers = args.iter().any(|a| a == "--speakers");
    settings.record_webcam = args.iter().any(|a| a == "--webcam");

    // Pick the monitor by index, or the primary one.
    let monitors = geometry::list_monitors();
    let monitor = match value(args, "--monitor") {
        Some(i) => monitors
            .get(i.parse::<usize>().context("--monitor")?)
            .cloned(),
        None => geometry::primary_monitor(),
    }
    .ok_or_else(|| anyhow!("no monitor found"))?;

    // Parse `--region X,Y,W,H` and clamp it to the monitor; default to the whole monitor.
    let region = match value(args, "--region") {
        Some(r) => {
            let v: Vec<i64> = r
                .split(',')
                .map(|p| p.trim().parse::<i64>())
                .collect::<Result<_, _>>()
                .context("--region X,Y,W,H")?;
            if v.len() != 4 {
                bail!("--region expects X,Y,W,H");
            }
            geometry::clamp_region(&monitor, v[0], v[1], v[2], v[3])
                .ok_or_else(|| anyhow!("region is outside the monitor"))?
        }
        None => monitor.full_region(),
    };

    let mut rec = Recorder::start(&settings, Some(region))?;
    for w in &rec.warnings {
        eprintln!("warning: {w}");
    }
    let duration = value(args, "--duration")
        .map(|d| d.parse::<f64>())
        .transpose()
        .context("--duration")?;
    // Record for a fixed time, checking for video failures, or until Enter is pressed.
    match duration {
        Some(d) => {
            eprintln!("Recording {} for {d} s…", region.describe());
            let end = Instant::now() + Duration::from_secs_f64(d);
            while Instant::now() < end {
                if let Some(e) = rec.video_error() {
                    bail!(e);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        None => {
            eprintln!("Recording {}… press Enter to stop.", region.describe());
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
        }
    }
    // Stop, apply `--output` instead of the templated file name, and produce the final file.
    let mut session = rec.stop()?;
    if let Some(out) = value(args, "--output") {
        session.manifest.output = PathBuf::from(out);
    }
    eprintln!("Saving…");
    let out = compose::finalize(&session, None)?;
    println!("{}", out.display());
    Ok(())
}

/// Prints the monitors (with the indices used by `--monitor`), microphones, recordable speaker
/// outputs, all audio inputs with their device ids, and webcams to stdout.
pub fn list_devices() {
    println!("Monitors:");
    for (i, m) in geometry::list_monitors().iter().enumerate() {
        println!(
            "  [{i}] {} {}x{} at ({}, {}) scale {}{}",
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            m.scale_factor,
            if m.is_primary { " (primary)" } else { "" }
        );
    }
    println!("Microphones:");
    for d in crate::audio::list_microphones() {
        println!("  {d}");
    }
    println!("Speakers (recordable outputs):");
    for d in crate::audio::list_speakers() {
        println!("  {d}");
    }
    println!("All audio inputs (debug):");
    for d in crate::audio::list_all_inputs() {
        println!("  {d}");
    }
    println!("Webcams:");
    for d in crate::media::mf::list_cameras() {
        println!("  {d}");
    }
}

/// `--selftest`: exercises every native component and prints an `[ OK ]` or `[FAIL]` line for
/// each, returning `true` if all checks passed.
///
/// Encodes a synthetic clip with the hardware and software encoders, decodes its video and
/// audio, runs the save steps (stream copy with added audio, GIF, MP3, AAC), measures 1080p
/// encoding speed and tests DXGI screen capture. Test files are left in a temporary folder whose
/// path is printed at the end. Set `SOFTROMEDA_SELFTEST_INPUT` to decode an existing file instead
/// of the encoded clip.
pub fn selftest() -> bool {
    use crate::media::{audio, convert, mf};
    use std::time::Instant;

    let dir = std::env::temp_dir().join(format!(
        "softromeda-screen-recorder-selftest-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    // Prints one check's result and remembers whether any check failed.
    let mut ok = true;
    let mut check = |name: &str, r: Result<String>| match r {
        Ok(detail) => println!("[ OK ] {name}: {detail}"),
        Err(e) => {
            ok = false;
            println!("[FAIL] {name}: {e:#}");
        }
    };

    check("Media Foundation", mf::init().map(|_| "started".into()));

    // Synthetic 2 s, 640 × 360 test material: red and light-grey vertical stripes that move left
    // by 8 pixels per frame, and a stereo 440 Hz tone.
    let (w, h, fps, secs) = (640usize, 360usize, 30.0, 2.0);
    let frame = |i: usize| -> Vec<u8> {
        let mut img = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 4;
                let on = (x + i * 8) % 160 < 80;
                img[p..p + 4].copy_from_slice(if on {
                    &[40, 40, 220, 255]
                } else {
                    &[230, 230, 230, 255]
                });
            }
        }
        img
    };
    let tone: Vec<f32> = (0..(secs * audio::RATE as f64) as usize)
        .flat_map(|i| {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / audio::RATE as f32).sin() * 0.3;
            [v, v]
        })
        .collect();

    // Encodes the test clip to `path` and checks that the result has the right length and an
    // audio track.
    let encode = |path: &std::path::Path, hardware: bool| -> Result<String> {
        let mut writer = mf::Writer::create(
            path,
            mf::WriterOptions {
                video: Some(mf::VideoInput::Encode(mf::VideoSpec {
                    width: w as u32,
                    height: h as u32,
                    fps,
                    bitrate: crate::media::video_bitrate(w as u32, h as u32, fps, 80),
                })),
                audio: Some(mf::AudioCodec::Aac),
                hardware,
                fragmented: false,
            },
        )?;
        let mut nv12 = vec![];
        let n = (secs * fps) as usize;
        for i in 0..n {
            convert::bgra_to_nv12(&frame(i), w * 4, w, h, &mut nv12);
            writer.write_nv12(&nv12, i as f64 / fps, 1.0 / fps)?;
            // Every 10 frames, write the audio covering those 10 frames so the streams are
            // interleaved (frame k starts at sample k * RATE / fps, times 2 for stereo).
            if i % 10 == 9 {
                let a = (i + 1 - 10) * audio::RATE as usize / fps as usize * 2;
                let b = ((i + 1) * audio::RATE as usize / fps as usize * 2).min(tone.len());
                writer.write_audio(&audio::to_i16(&tone[a..b]))?;
            }
        }
        writer.finish()?;
        let info = mf::probe(path)?;
        anyhow::ensure!(
            (info.duration - secs).abs() < 0.3,
            "duration {:.2} s",
            info.duration
        );
        anyhow::ensure!(info.has_audio, "no audio track");
        Ok(format!(
            "{}x{} {:.2} s, {} KB",
            info.width,
            info.height,
            info.duration,
            std::fs::metadata(path)?.len() / 1024
        ))
    };
    let hw_path = dir.join("hardware.mp4");
    check(
        "H.264/AAC encode (hardware allowed)",
        encode(&hw_path, true),
    );
    let sw_path = dir.join("software.mp4");
    check("H.264/AAC encode (software)", encode(&sw_path, false));

    // Developer aid: decode an existing file instead, e.g. when encoding is unavailable.
    let sw_path = std::env::var_os("SOFTROMEDA_SELFTEST_INPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or(sw_path);
    check(
        "Decode video",
        (|| {
            let mut r = mf::VideoReader::open(&sw_path, true)?;
            let mut n = 0;
            let mut first = None;
            while let Some(f) = r.read_frame()? {
                if first.is_none() {
                    first = Some(f);
                }
                n += 1;
            }
            let f = first.ok_or_else(|| anyhow!("no frames"))?;
            // The top-left pixel must be from a red stripe (BGRA 40, 40, 220); anything else
            // means the image was decoded upside down or with wrong colours.
            let px = &f.bgra[0..4];
            anyhow::ensure!(
                px[2] > 150 && px[0] < 120,
                "unexpected colour {px:?} (flipped?)"
            );
            Ok(format!("{n} frames, {}x{}", f.width, f.height))
        })(),
    );
    check(
        "Decode audio",
        (|| {
            let mut r = mf::AudioReader::open(&sw_path)?;
            let mut samples = 0;
            while let Some((_, c)) = r.read_chunk()? {
                samples += c.len();
            }
            Ok(format!(
                "{} Hz, {} ch, {samples} samples",
                r.rate, r.channels
            ))
        })(),
    );
    // The same stream copy that `compose` uses to save an MP4 recording.
    check(
        "Copy video + add audio (save step)",
        (|| {
            let out = dir.join("remux.mp4");
            let mut reader = mf::VideoReader::open(&sw_path, false)?;
            let native = reader.native_type()?;
            let mut writer = mf::Writer::create(
                &out,
                mf::WriterOptions {
                    video: Some(mf::VideoInput::Passthrough(&native)),
                    audio: Some(mf::AudioCodec::Aac),
                    hardware: false,
                    fragmented: false,
                },
            )?;
            writer.write_audio(&audio::to_i16(&tone))?;
            while let Some(s) = reader.read_compressed()? {
                writer.write_video_sample(&s)?;
            }
            writer.finish()?;
            let info = mf::probe(&out)?;
            Ok(format!("{:.2} s, audio: {}", info.duration, info.has_audio))
        })(),
    );
    check(
        "Animated GIF",
        crate::compose::write_gif(&sw_path, &dir.join("test.gif"), 10.0, None).and_then(|_| {
            Ok(format!(
                "{} KB",
                std::fs::metadata(dir.join("test.gif"))?.len() / 1024
            ))
        }),
    );
    check(
        "MP3 encode",
        crate::compose::write_audio_file(&dir.join("test.mp3"), mf::AudioCodec::Mp3, &tone, None)
            .map(|_| "ok".into()),
    );
    check(
        "AAC (.m4a) encode",
        crate::compose::write_audio_file(&dir.join("test.m4a"), mf::AudioCodec::Aac, &tone, None)
            .map(|_| "ok".into()),
    );

    // Encoding speed at 1080p decides how smooth 60 fps recordings are. Each frame differs, so
    // the encoder cannot skip work.
    for hardware in [true, false] {
        check(
            if hardware {
                "1080p encode speed (hardware allowed)"
            } else {
                "1080p encode speed (software)"
            },
            (|| {
                let (bw, bh) = (1920usize, 1080usize);
                let path = dir.join(format!("speed-{hardware}.mp4"));
                let writer = mf::Writer::create(
                    &path,
                    mf::WriterOptions {
                        video: Some(mf::VideoInput::Encode(mf::VideoSpec {
                            width: bw as u32,
                            height: bh as u32,
                            fps: 60.0,
                            bitrate: crate::media::video_bitrate(bw as u32, bh as u32, 60.0, 100),
                        })),
                        audio: None,
                        hardware,
                        fragmented: true,
                    },
                )?;
                let mut img = vec![0u8; bw * bh * 4];
                let mut nv12 = vec![];
                let t0 = Instant::now();
                let n = 120;
                for i in 0..n {
                    for (k, px) in img.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        px[0] = ((k + i * 7) % 255) as u8;
                    }
                    convert::bgra_to_nv12(&img, bw * 4, bw, bh, &mut nv12);
                    writer.write_nv12(&nv12, i as f64 / 60.0, 1.0 / 60.0)?;
                }
                writer.finish()?;
                let fps = n as f64 / t0.elapsed().as_secs_f64();
                Ok(format!("{fps:.0} frames/s (needs 60 for smooth 60 fps)"))
            })(),
        );
    }

    // Count screen updates in a 256 × 256 corner of the primary monitor for one second.
    check(
        "DXGI screen capture",
        (|| {
            let m = crate::geometry::primary_monitor().ok_or_else(|| anyhow!("no monitor"))?;
            let mut d = crate::media::capture::Duplicator::new((m.x, m.y), (0, 0, 256, 256))?;
            let t0 = Instant::now();
            let mut images = 0;
            while t0.elapsed().as_secs_f64() < 1.0 {
                if let crate::media::capture::Grab::Frame(_) = d.grab(100)? {
                    images += 1;
                }
            }
            Ok(format!(
                "working ({images} screen updates in 1 s; a still screen gives 0-1)"
            ))
        })(),
    );
    check("Webcams", Ok(format!("{:?}", mf::list_cameras())));

    println!("Test files: {}", dir.display());
    ok
}
