// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! Pixel format conversion, scaling and compositing for 32-bit BGRA / RGBA frames.
//!
//! Screen frames arrive from DXGI as BGRA, which Media Foundation calls RGB32. Before encoding
//! they are converted to NV12 (BT.709, limited range) on the CPU across all cores with rayon;
//! this takes about 1–2 ms for 1080p and avoids the quirks of Media Foundation's colour
//! converter (bottom-up RGB). Decoded NV12 is converted back with [`nv12_to_bgra`].
//!
//! The remaining functions (resize, crop, blit, letterbox, blur, blend, fade) are the image
//! operations used by the recorder and the editor's renderer. All images are tightly packed,
//! 4 bytes per pixel, and the functions preserve the channel order unless stated otherwise.

use rayon::prelude::*;

/// Converts a BGRA image (rows `stride` bytes apart) into NV12 in `dst`, which is resized to
/// `width * height * 3 / 2` bytes.
///
/// Uses BT.709 limited-range coefficients in 8-bit fixed point; chroma is the average of each
/// 2×2 block. `width` and `height` must be even.
pub fn bgra_to_nv12(src: &[u8], stride: usize, width: usize, height: usize, dst: &mut Vec<u8>) {
    debug_assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
    let y_size = width * height;
    dst.resize(y_size + y_size / 2, 0);
    let (y_plane, uv_plane) = dst.split_at_mut(y_size);

    // Each task handles two luma rows and the chroma row between them.
    y_plane
        .par_chunks_mut(width * 2)
        .zip(uv_plane.par_chunks_mut(width))
        .enumerate()
        .for_each(|(pair, (y_rows, uv_row))| {
            let r0 = &src[pair * 2 * stride..];
            let r1 = &src[(pair * 2 + 1) * stride..];
            let (y0, y1) = y_rows.split_at_mut(width);
            // One 2×2 block per step: four luma samples and one interleaved U/V pair.
            // The coefficients are the BT.709 weights scaled by 256 and by 219/255 (luma) or
            // 224/255 (chroma) for limited range; `+ 128` rounds before the shift.
            for x in (0..width).step_by(2) {
                let mut rs = 0i32;
                let mut gs = 0i32;
                let mut bs = 0i32;
                for (row, yrow) in [(r0, &mut *y0), (r1, &mut *y1)] {
                    for dx in 0..2 {
                        let p = (x + dx) * 4;
                        let (b, g, r) = (row[p] as i32, row[p + 1] as i32, row[p + 2] as i32);
                        yrow[x + dx] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
                        rs += r;
                        gs += g;
                        bs += b;
                    }
                }
                // Chroma from the block's average colour.
                let (r, g, b) = (rs / 4, gs / 4, bs / 4);
                let u = ((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
                uv_row[x] = u.clamp(0, 255) as u8;
                uv_row[x + 1] = v.clamp(0, 255) as u8;
            }
        });
}

/// Converts an NV12 image to opaque BGRA.
///
/// The input is limited range; BT.709 is assumed for HD (height 720 and above) and BT.601 for
/// SD, matching what encoders usually tag. `y_stride` is the byte distance between rows of both
/// planes, and the interleaved UV plane starts at byte `uv_offset` of `src`.
pub fn nv12_to_bgra(
    src: &[u8],
    y_stride: usize,
    uv_offset: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    // Fixed-point (×256) coefficients for the colour difference terms.
    let (rv, gu, gv, bu) = if height >= 720 {
        (459, 55, 136, 541) // BT.709
    } else {
        (409, 100, 208, 516) // BT.601
    };
    let mut out = vec![0u8; width * height * 4];
    out.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let yrow = &src[y * y_stride..];
            let uvrow = &src[uv_offset + (y / 2) * y_stride..];
            // Expand limited-range luma (×298/256 ≈ 255/219) and add the shared chroma of
            // each horizontal pixel pair.
            for x in 0..width {
                let c = (yrow[x] as i32 - 16).max(0) * 298;
                let u = uvrow[x & !1] as i32 - 128;
                let v = uvrow[(x & !1) + 1] as i32 - 128;
                let r = (c + rv * v + 128) >> 8;
                let g = (c - gu * u - gv * v + 128) >> 8;
                let b = (c + bu * u + 128) >> 8;
                let p = x * 4;
                row[p] = b.clamp(0, 255) as u8;
                row[p + 1] = g.clamp(0, 255) as u8;
                row[p + 2] = r.clamp(0, 255) as u8;
                row[p + 3] = 255;
            }
        });
    out
}

/// Swaps the red and blue channels (RGBA ↔ BGRA) of every pixel in place.
pub fn swap_red_blue(pixels: &mut [u8]) {
    pixels.par_chunks_mut(4 * 1024).for_each(|chunk| {
        for px in chunk.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    });
}

/// Resizes a 32-bit image from `sw`×`sh` to `dw`×`dh` pixels with bilinear filtering.
///
/// Pixel centres are aligned (half-pixel offset) and edges are clamped. Returns a copy when the
/// sizes match and a zeroed image when either size is empty.
pub fn resize(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let mut out = vec![0u8; dw * dh * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    // Precompute, for each destination column, the two source columns and the 8-bit weight of
    // the right one; rows are handled the same way inside the parallel loop.
    let xs: Vec<(usize, usize, u32)> = (0..dw)
        .map(|x| {
            let fx = ((x as f64 + 0.5) * sw as f64 / dw as f64 - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            (x0, x1, ((fx - x0 as f64) * 256.0) as u32)
        })
        .collect();
    out.par_chunks_mut(dw * 4).enumerate().for_each(|(y, row)| {
        let fy = ((y as f64 + 0.5) * sh as f64 / dh as f64 - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = ((fy - y0 as f64) * 256.0) as u32;
        let r0 = &src[y0 * sw * 4..];
        let r1 = &src[y1 * sw * 4..];
        // Interpolate horizontally on both rows, then vertically; the result is scaled by
        // 256 × 256, hence the rounding shift by 16.
        for (x, &(x0, x1, wx)) in xs.iter().enumerate() {
            for c in 0..4 {
                let a = r0[x0 * 4 + c] as u32;
                let b = r0[x1 * 4 + c] as u32;
                let cc = r1[x0 * 4 + c] as u32;
                let d = r1[x1 * 4 + c] as u32;
                let top = a * (256 - wx) + b * wx;
                let bottom = cc * (256 - wx) + d * wx;
                row[x * 4 + c] = ((top * (256 - wy) + bottom * wy + 32768) >> 16) as u8;
            }
        }
    });
    out
}

/// Copies the `w`×`h` rectangle at (`x`, `y`) out of a 32-bit image that is `sw` pixels wide.
///
/// Panics if the rectangle extends beyond the image.
pub fn crop(src: &[u8], sw: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = (row * sw + x) * 4;
        out.extend_from_slice(&src[start..start + w * 4]);
    }
    out
}

/// Copies `src` (`sw`×`sh`) into `dst` (`dw`×`dh`) with its top-left corner at (`x`, `y`),
/// clipped to `dst`. Pixels are copied without blending.
#[allow(clippy::too_many_arguments)]
pub fn blit(
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    src: &[u8],
    sw: usize,
    sh: usize,
    x: i64,
    y: i64,
) {
    // Copy each visible source row as one slice, skipping rows above or below `dst` and
    // trimming columns outside it.
    for row in 0..sh as i64 {
        let ty = y + row;
        if ty < 0 || ty >= dh as i64 {
            continue;
        }
        let x0 = x.max(0);
        let x1 = (x + sw as i64).min(dw as i64);
        if x0 >= x1 {
            continue;
        }
        let s = ((row as usize) * sw + (x0 - x) as usize) * 4;
        let d = (ty as usize * dw + x0 as usize) * 4;
        let n = (x1 - x0) as usize * 4;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

/// Like [`blit`], but with anti-aliased rounded corners of `radius` pixels.
///
/// Pixels outside the rounded shape keep the background; edge pixels blend the colour channels
/// by their coverage (the alpha channel of `dst` is left unchanged there). The radius is limited
/// to half the shorter side. Used for the webcam overlay.
#[allow(clippy::too_many_arguments)]
pub fn blit_rounded(
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    src: &[u8],
    sw: usize,
    sh: usize,
    x: i64,
    y: i64,
    radius: f32,
) {
    let r = radius.clamp(0.0, sw.min(sh) as f32 / 2.0);
    for row in 0..sh {
        let ty = y + row as i64;
        if ty < 0 || ty >= dh as i64 {
            continue;
        }
        for col in 0..sw {
            let tx = x + col as i64;
            if tx < 0 || tx >= dw as i64 {
                continue;
            }
            // Clamping the pixel centre to the inner rectangle gives the nearest corner-circle
            // centre; the distance from it decides the coverage (zero away from corners).
            let px = col as f32 + 0.5;
            let py = row as f32 + 0.5;
            let cx = px.clamp(r, sw as f32 - r);
            let cy = py.clamp(r, sh as f32 - r);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let cover = (r - d + 0.5).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let s = (row * sw + col) * 4;
            let o = (ty as usize * dw + tx as usize) * 4;
            if cover >= 1.0 {
                dst[o..o + 4].copy_from_slice(&src[s..s + 4]);
            } else {
                for c in 0..3 {
                    dst[o + c] = (src[s + c] as f32 * cover + dst[o + c] as f32 * (1.0 - cover))
                        .round() as u8;
                }
            }
        }
    }
}

/// Scales `src` (`sw`×`sh`) to fit inside `dw`×`dh` keeping its aspect ratio, centred on an
/// opaque black background.
///
/// Used to keep the video size constant when a recorded window is resized.
pub fn letterbox(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let scale = (dw as f64 / sw as f64).min(dh as f64 / sh as f64);
    let w = ((sw as f64 * scale).round() as usize).clamp(1, dw);
    let h = ((sh as f64 * scale).round() as usize).clamp(1, dh);
    let scaled = resize(src, sw, sh, w, h);
    // Opaque black canvas.
    let mut out = vec![0u8; dw * dh * 4];
    for px in out.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    blit(
        &mut out,
        dw,
        dh,
        &scaled,
        w,
        h,
        ((dw - w) / 2) as i64,
        ((dh - h) / 2) as i64,
    );
    out
}

/// Blurs the rectangle (`rx`, `ry`, `rw`, `rh`) of a `w`×`h` image in place.
///
/// The rectangle is clipped to the image; `radius` is the box radius in pixels (0 does nothing).
/// Used for the editor's blur annotation, which hides sensitive information.
#[allow(clippy::too_many_arguments)]
pub fn blur_rect(
    img: &mut [u8],
    w: usize,
    h: usize,
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    radius: usize,
) {
    let rx = rx.min(w);
    let ry = ry.min(h);
    let rw = rw.min(w - rx);
    let rh = rh.min(h - ry);
    if rw == 0 || rh == 0 || radius == 0 {
        return;
    }
    let mut region = crop(img, w, rx, ry, rw, rh);
    // Blur a copy of the region with two rounds of a separable box blur (close to a Gaussian),
    // then put it back.
    for _ in 0..2 {
        region = box_pass(&region, rw, rh, radius, true);
        region = box_pass(&region, rw, rh, radius, false);
    }
    blit(img, w, h, &region, rw, rh, rx as i64, ry as i64);
}

/// One box-blur pass of radius `r` over all four channels of a `w`×`h` image, along rows
/// (`horizontal`) or columns.
///
/// Uses a running sum, so the cost does not depend on the radius; samples beyond the edges
/// repeat the edge pixel.
fn box_pass(src: &[u8], w: usize, h: usize, r: usize, horizontal: bool) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let idx = |line: usize, i: usize| {
        if horizontal {
            (line * w + i) * 4
        } else {
            (i * w + line) * 4
        }
    };
    for line in 0..lines {
        for c in 0..4 {
            // Prime the sum with the window centred on the first pixel.
            let mut acc: u32 = 0;
            let window = (2 * r + 1) as u32;
            for k in 0..=2 * r {
                let i = (k as i64 - r as i64).clamp(0, len as i64 - 1) as usize;
                acc += src[idx(line, i) + c] as u32;
            }
            // Slide the window: add the sample entering on the right, drop the one leaving.
            for i in 0..len {
                out[idx(line, i) + c] = (acc / window) as u8;
                let add = (i + r + 1).min(len - 1);
                let sub = (i as i64 - r as i64).max(0) as usize;
                acc += src[idx(line, add) + c] as u32;
                acc -= src[idx(line, sub) + c] as u32;
            }
        }
    }
    out
}

/// Alpha-blends a straight (non-premultiplied) alpha overlay of the same size and channel order
/// onto `dst`.
///
/// Fully transparent overlay pixels are skipped; blended pixels become opaque.
pub fn blend_over(dst: &mut [u8], overlay: &[u8]) {
    dst.par_chunks_mut(4 * 1024)
        .zip(overlay.par_chunks(4 * 1024))
        .for_each(|(d, o)| {
            for (dp, op) in d
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(o.as_chunks::<4>().0)
            {
                let a = op[3] as u32;
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                for c in 0..3 {
                    dp[c] = ((op[c] as u32 * a + dp[c] as u32 * inv + 127) / 255) as u8;
                }
                dp[3] = 255;
            }
        });
}

/// Darkens every pixel towards black by multiplying its colour by `level` (0..=1).
///
/// A level of 1 or more leaves the image unchanged; alpha is not modified. Used for fade in and
/// fade out.
pub fn fade(img: &mut [u8], level: f32) {
    if level >= 1.0 {
        return;
    }
    let k = (level.clamp(0.0, 1.0) * 256.0) as u32;
    img.par_chunks_mut(4 * 1024).for_each(|chunk| {
        for px in chunk.as_chunks_mut::<4>().0 {
            for c in px.iter_mut().take(3) {
                *c = ((*c as u32 * k) >> 8) as u8;
            }
        }
    });
}

/// Darkens everything outside a soft-edged circle, like a spotlight.
///
/// `img` holds `w` × `h` 32-bit pixels (BGRA or RGBA; alpha is left alone). The circle is centred
/// on (`cx`, `cy`) in pixels with the given `radius`; its edge fades over a further 35 % of the
/// radius. Outside it, colours keep `1 - dim` of their brightness (`dim` is 0..=1).
pub fn spotlight(img: &mut [u8], w: usize, h: usize, cx: f32, cy: f32, radius: f32, dim: f32) {
    if w == 0 || h == 0 || img.len() < w * h * 4 {
        return;
    }
    let radius = radius.max(1.0);
    let feather = (radius * 0.35).max(2.0);
    let (inner, outer) = (radius * radius, (radius + feather) * (radius + feather));
    // Brightness kept outside the circle, in 1/256 steps.
    let keep = ((1.0 - dim.clamp(0.0, 1.0)) * 256.0) as u32;
    img.par_chunks_mut(w * 4)
        .take(h)
        .enumerate()
        .for_each(|(y, row)| {
            let dy = y as f32 + 0.5 - cy;
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let dx = x as f32 + 0.5 - cx;
                let d2 = dx * dx + dy * dy;
                let k = if d2 <= inner {
                    continue; // fully lit
                } else if d2 >= outer {
                    keep
                } else {
                    // Smooth edge between the lit circle and the dimmed rest.
                    let t = (d2.sqrt() - radius) / feather;
                    let t = t * t * (3.0 - 2.0 * t);
                    (256.0 - (256 - keep) as f32 * t) as u32
                };
                for c in &mut px[..3] {
                    *c = ((*c as u32 * k) >> 8) as u8;
                }
            }
        });
}

#[cfg(test)]
mod tests {
    /// The spotlight keeps the centre, dims the outside and fades in between.
    #[test]
    fn spotlight_dims_outside_the_circle() {
        let (w, h) = (100usize, 60usize);
        let mut img = vec![200u8; w * h * 4];
        super::spotlight(&mut img, w, h, 50.0, 30.0, 10.0, 0.5);
        let px = |x: usize, y: usize| img[(y * w + x) * 4];
        assert_eq!(px(50, 30), 200, "centre untouched");
        assert_eq!(px(5, 5), 100, "outside dimmed by half");
        let edge = px(62, 30); // between radius 10 and 13.5
        assert!(edge > 100 && edge < 200, "soft edge: {edge}");
        assert_eq!(img[3], 200, "alpha untouched");
    }

    use super::*;

    /// White and black map to limited-range luma 235 / 16 with neutral chroma.
    #[test]
    fn nv12_of_known_colours() {
        // 2x2 white, 2x2 black side by side, as BGRA.
        let mut src = vec![];
        for _ in 0..2 {
            src.extend_from_slice(&[
                255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255,
            ]);
        }
        let mut out = vec![];
        bgra_to_nv12(&src, 16, 4, 2, &mut out);
        assert_eq!(out.len(), 12);
        assert_eq!(&out[0..4], &[235, 235, 16, 16]); // limited-range luma
        assert_eq!(&out[8..12], &[128, 128, 128, 128]); // neutral chroma
    }

    /// Pure red gives the expected luma, low U and high V.
    #[test]
    fn nv12_red_has_high_v() {
        let src: Vec<u8> = [0u8, 0, 255, 255].repeat(4); // red BGRA
        let mut out = vec![];
        bgra_to_nv12(&src, 8, 2, 2, &mut out);
        let (y, u, v) = (out[0], out[4], out[5]);
        assert!((60..=66).contains(&y), "y {y}");
        assert!(u < 110 && v > 230, "u {u} v {v}");
    }

    /// A colour survives BGRA → NV12 → BGRA (HD, BT.709 both ways) within rounding error.
    #[test]
    fn nv12_round_trip() {
        let mut src = vec![];
        for _ in 0..720 / 2 * 2 {
            src.extend_from_slice(&[30, 60, 200, 255, 30, 60, 200, 255]);
        }
        let (w, h) = (2usize, 720usize);
        let mut nv12 = vec![];
        bgra_to_nv12(&src, w * 4, w, h, &mut nv12);
        let back = nv12_to_bgra(&nv12, w, w * h, w, h);
        for c in 0..3 {
            let d = (back[c] as i32 - src[c] as i32).abs();
            assert!(d <= 4, "channel {c}: {} vs {}", back[c], src[c]);
        }
    }

    /// Same-size resize is a copy, downscaling gives the right size, and crop picks the right
    /// pixels.
    #[test]
    fn resize_and_crop() {
        let src: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        assert_eq!(resize(&src, 4, 4, 4, 4), src);
        let half = resize(&src, 4, 4, 2, 2);
        assert_eq!(half.len(), 16);
        let c = crop(&src, 4, 1, 1, 2, 2);
        assert_eq!(&c[0..4], &src[20..24]);
    }

    /// A wide image letterboxed into a square gets black bars above and below.
    #[test]
    fn letterbox_keeps_aspect() {
        let src = vec![255u8; 4 * 2 * 4]; // 4x2 white
        let out = letterbox(&src, 4, 2, 4, 4);
        // Top row black, middle rows white.
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4 * 4 * 2..4 * 4 * 2 + 4], &[255, 255, 255, 255]);
    }

    /// Rounded blitting leaves the corner pixel untouched and fully covers the interior and edges.
    #[test]
    fn rounded_corners_keep_background() {
        let (w, h) = (20usize, 20usize);
        let mut dst = vec![0u8; w * h * 4];
        let src = vec![255u8; w * h * 4];
        blit_rounded(&mut dst, w, h, &src, w, h, 0, 0, 6.0);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]); // corner stays background
        let centre = (10 * w + 10) * 4;
        assert_eq!(&dst[centre..centre + 3], &[255, 255, 255]);
        let edge = (10 * w) * 4; // middle of the left edge is fully covered
        assert_eq!(dst[edge], 255);
    }

    /// Half-transparent white over black gives mid-grey; fading by 0.5 halves the colour.
    #[test]
    fn blend_and_fade() {
        let mut dst = vec![0u8, 0, 0, 255];
        blend_over(&mut dst, &[255, 255, 255, 128]);
        assert!((127..=129).contains(&dst[0]));
        let mut img = vec![200u8, 200, 200, 255];
        fade(&mut img, 0.5);
        assert_eq!(img[0], 100);
    }

    /// Blurring a hard black/white edge produces an intermediate value at the boundary.
    #[test]
    fn blur_smooths_edge() {
        let (w, h) = (8, 1);
        let mut img = vec![0u8; w * h * 4];
        for x in 4..8 {
            img[x * 4..x * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        blur_rect(&mut img, w, h, 0, 0, w, h, 1);
        assert!(img[3 * 4] > 0 && img[3 * 4] < 255);
    }
}
