// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Software pointer cursor — compositor-owned scene element.
//!
//! The classic two-tone arrow (black outline, white fill) drawn
//! into the CPU composite like any other scene element: same
//! coordinate space, same clipping, no side channel. The hardware
//! cursor plane replaces this routine later behind an identical
//! call signature — scanout-level cursors are a pure performance
//! upgrade, not a semantic change.
//!
//! Clients' `wl_pointer.set_cursor` surfaces override this
//! default once cursor-surface compositing lands; a compositor
//! must always have a fallback image of its own.

/// Arrow glyph, 12×20, hotspot at (0,0). `X` = black outline,
/// `O` = white fill — the two-tone convention every X11/Wayland
/// default cursor uses so the pointer reads on any backdrop.
const ARROW: [&str; 19] = [
    "X...........",
    "XX..........",
    "XOX.........",
    "XOOX........",
    "XOOOX.......",
    "XOOOOX......",
    "XOOOOOX.....",
    "XOOOOOOX....",
    "XOOOOOOOX...",
    "XOOOOOOOOX..",
    "XOOOOOXXXXX.",
    "XOOXOOX.....",
    "XOX.XOOX....",
    "XX..XOOX....",
    "X...XOOX....",
    "....XOOX....",
    ".....XOX....",
    "......XX....",
    "............",
];

/// Src-over blit of a themed cursor image — the wp_cursor_shape
/// path hands us Xcursor pixels (ARGB32 premultiplied, which in
/// memory is the same B-G-R-A byte order the frame uses).
/// (x, y) is the image's top-left in output-local pixels.
pub fn blit_img(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    img: &crate::xcursor::CursorImg,
    x: i32,
    y: i32,
) {
    let (fw, fh) = (fw as i32, fh as i32);
    for row in 0..img.h as i32 {
        let dy = y + row;
        if dy < 0 || dy >= fh {
            continue;
        }
        for col in 0..img.w as i32 {
            let dx = x + col;
            if dx < 0 || dx >= fw {
                continue;
            }
            let si = (row as usize * img.w as usize + col as usize) * 4;
            let di = (dy as usize * fw as usize + dx as usize) * 4;
            let a = img.pixels[si + 3] as u32;
            if a == 0 {
                continue;
            }
            if a == 255 {
                frame[di..di + 4].copy_from_slice(&img.pixels[si..si + 4]);
                continue;
            }
            let inv = 255 - a;
            for c in 0..3 {
                let s = img.pixels[si + c] as u32;
                let d = frame[di + c] as u32;
                frame[di + c] = ((s * a + d * inv) / 255) as u8;
            }
            frame[di + 3] = 0xff;
        }
    }
}

/// Draw the cursor at output-local position (x, y), clipped to
/// the destination frame. `frame` is packed BGRA8888 like every
/// other buffer this compositor produces.
pub fn draw(frame: &mut [u8], fw: u32, fh: u32, x: i32, y: i32) {
    let (fw, fh) = (fw as i32, fh as i32);
    for (row, line) in ARROW.iter().enumerate() {
        let dy = y + row as i32;
        if dy < 0 || dy >= fh {
            continue;
        }
        for (col, ch) in line.bytes().enumerate() {
            let dx = x + col as i32;
            if dx < 0 || dx >= fw {
                continue;
            }
            let di = (dy as usize * fw as usize + dx as usize) * 4;
            match ch {
                b'X' => frame[di..di + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0xff]),
                b'O' => frame[di..di + 4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]),
                _ => {}
            }
        }
    }
}
