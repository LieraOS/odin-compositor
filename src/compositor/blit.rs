// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! CPU blit helpers — src-over compositing of client shm buffers
//! into an output frame. `blit` is the 1:1 path; `blit_scaled`
//! implements the wp_viewporter crop+scale transform.
//!
//! Both functions resolve the drawable rectangle ONCE — surface
//! ∩ output ∩ damage clip — and iterate only that range, with
//! source bounds folded into per-row limits. An earlier version
//! scanned every pixel of every mapped surface and skipped
//! out-of-clip pixels inside the inner loop: a fullscreen
//! window turned each cursor move into a multi-million-pixel
//! pass, which is why pointer motion stalled with any client
//! open.

use super::BufInfo;

/// Src-over blit of one shm buffer into a frame; `dx/dy` is the
/// buffer's top-left in output-local pixels.
pub fn blit(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    src: *const u8,
    len: i64,
    info: &BufInfo,
    dx: i32,
    dy: i32,
    clip: Option<(i32, i32, i32, i32)>,
    alpha: f32,
) {
    // Drawable rect = surface ∩ output bounds ∩ damage clip.
    let mut x0 = dx.max(0);
    let mut y0 = dy.max(0);
    let mut x1 = (dx + info.w).min(fw as i32);
    let mut y1 = (dy + info.h).min(fh as i32);
    if let Some((cx, cy, cw, ch)) = clip {
        x0 = x0.max(cx);
        y0 = y0.max(cy);
        x1 = x1.min(cx + cw);
        y1 = y1.min(cy + ch);
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // wp_alpha_modifier — the committed multiplier lands as a
    // 0..255 factor applied to the source alpha before blending.
    let am = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
    for py in y0..y1 {
        let row = info.offset + (py - dy) * info.stride;
        // Fold the shm tail bound into this row's column count —
        // no per-pixel source check needed beyond it.
        let mut si = row + (x0 - dx) * 4;
        let cols = ((x1 - x0) as i64).min((len - si as i64) / 4).max(0) as i32;
        let mut di = (py as usize * fw as usize + x0 as usize) * 4;
        unsafe {
            for _ in 0..cols {
                // Src-over blend over the opaque backdrop — a raw
                // copy kept client alpha <255, letting the host
                // bleed through semi-transparent surfaces.
                let a = *src.add(si as usize + 3) as u32 * am / 255;
                if a == 255 {
                    // Opaque src-over is a plain overwrite — the
                    // common case for window content.
                    frame[di..di + 4].copy_from_slice(&[
                        *src.add(si as usize),
                        *src.add(si as usize + 1),
                        *src.add(si as usize + 2),
                        0xff,
                    ]);
                } else if a > 0 {
                    let inv = 255 - a;
                    for c in 0..3 {
                        let s = *src.add(si as usize + c) as u32;
                        let d = frame[di + c] as u32;
                        frame[di + c] = ((s * a + d * inv) / 255) as u8;
                    }
                    frame[di + 3] = 0xff;
                }
                si += 4;
                di += 4;
            }
        }
    }
}

/// Scaled+cropped src-over blit — the wp_viewporter path. The
/// buffer region `(sx, sy, sw, sh)` maps onto `(dw, dh)` output
/// pixels via nearest-neighbor sampling; fine for a CPU
/// fallback, and exact when the scale is an integer ratio.
#[allow(clippy::too_many_arguments)]
pub fn blit_scaled(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    src: *const u8,
    len: i64,
    info: &BufInfo,
    dx: i32,
    dy: i32,
    src_rect: (f64, f64, f64, f64),
    dst: (i32, i32),
    clip: Option<(i32, i32, i32, i32)>,
    alpha: f32,
) {
    let (sx0, sy0, sw, sh) = src_rect;
    let (dw, dh) = dst;
    if dw <= 0 || dh <= 0 || sw <= 0.0 || sh <= 0.0 {
        return;
    }
    // Drawable rect = dest rect ∩ output bounds ∩ damage clip.
    let mut x0 = dx.max(0);
    let mut y0 = dy.max(0);
    let mut x1 = (dx + dw).min(fw as i32);
    let mut y1 = (dy + dh).min(fh as i32);
    if let Some((cx, cy, cw, ch)) = clip {
        x0 = x0.max(cx);
        y0 = y0.max(cy);
        x1 = x1.min(cx + cw);
        y1 = y1.min(cy + ch);
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // wp_alpha_modifier — same multiplier semantics as `blit`.
    let am = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
    let (fx, fy) = (sw / dw as f64, sh / dh as f64);
    for py in y0..y1 {
        let syy = (sy0 + (py - dy) as f64 * fy) as i32;
        if syy < 0 || syy >= info.h {
            continue;
        }
        let row = info.offset + syy * info.stride;
        for px in x0..x1 {
            let sxx = (sx0 + (px - dx) as f64 * fx) as i32;
            if sxx < 0 || sxx >= info.w {
                continue;
            }
            let si = (row + sxx * 4) as i64;
            if si < 0 || si + 4 > len {
                continue;
            }
            let di = (py as usize * fw as usize + px as usize) * 4;
            unsafe {
                let a = *src.add(si as usize + 3) as u32 * am / 255;
                if a == 255 {
                    frame[di..di + 4].copy_from_slice(&[
                        *src.add(si as usize),
                        *src.add(si as usize + 1),
                        *src.add(si as usize + 2),
                        0xff,
                    ]);
                } else if a > 0 {
                    let inv = 255 - a;
                    for c in 0..3 {
                        let s = *src.add(si as usize + c) as u32;
                        let d = frame[di + c] as u32;
                        frame[di + c] = ((s * a + d * inv) / 255) as u8;
                    }
                    frame[di + 3] = 0xff;
                }
            }
        }
    }
}
