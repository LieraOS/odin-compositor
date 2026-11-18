// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! XCursor theme loading — server-side cursor images for the
//! wp_cursor_shape protocol and the compositor fallback pointer.
//!
//! The XCursor file format: magic "Xcur", a header, a table of
//! contents pointing at image chunks, and each image chunk holds
//! nominal size, real dimensions, hotspot, frame delay and a
//! width×height ARGB32 pixel array (premultiplied, A in the top
//! byte — same layout wl_shm ARGB8888 expects).

/// One resolved cursor frame — pixels plus its hotspot.
#[derive(Clone)]
pub struct CursorImg {
    pub w: u32,
    pub h: u32,
    pub hx: i32,
    pub hy: i32,
    /// Frame delay in ms — cursors may be animated; the first
    /// frame is what we composite, delays are kept for a future
    /// animated cursor path.
    #[allow(dead_code)]
    pub delay_ms: u32,
    /// ARGB32 premultiplied pixels, row-major.
    pub pixels: Vec<u8>,
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Parse an Xcursor file — returns the image frame whose nominal
/// size is closest to `want` (the common 24 px at scale 1).
pub fn parse(data: &[u8], want: u32) -> Option<CursorImg> {
    if data.len() < 16 || &data[0..4] != b"Xcur" {
        return None;
    }
    let ntoc = u32le(data, 12) as usize;
    // TOC entries: (type, subtype, position) × ntoc — image
    // chunks carry type 0xfffd0002.
    let mut best: Option<(u32, usize)> = None;
    for i in 0..ntoc {
        let o = 16 + i * 12;
        if o + 12 > data.len() {
            break;
        }
        let (ty, sub, pos) = (
            u32le(data, o),
            u32le(data, o + 4),
            u32le(data, o + 8) as usize,
        );
        if ty != 0xfffd_0002 {
            continue;
        }
        let better = match best {
            None => true,
            Some((bs, _)) => sub.abs_diff(want) < bs.abs_diff(want),
        };
        if better {
            best = Some((sub, pos));
        }
    }
    let (_, pos) = best?;
    // Image chunk header: 9 × u32 then pixels.
    if pos + 36 > data.len() {
        return None;
    }
    let (w, h) = (u32le(data, pos + 16), u32le(data, pos + 20));
    let (hx, hy) = (u32le(data, pos + 24) as i32, u32le(data, pos + 28) as i32);
    let delay = u32le(data, pos + 32);
    let px_off = pos + 36;
    let px_len = (w as usize) * (h as usize) * 4;
    if px_off + px_len > data.len() {
        return None;
    }
    Some(CursorImg {
        w,
        h,
        hx,
        hy,
        delay_ms: delay,
        pixels: data[px_off..px_off + px_len].to_vec(),
    })
}

/// Xcursor search roots, in priority order — $XCURSOR_PATH, the
/// user's icon dir, then the system icon dirs.
fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = std::env::var("XCURSOR_PATH")
        .map(|p| p.split(':').map(std::path::PathBuf::from).collect())
        .unwrap_or_default();
    if let Ok(home) = std::env::var("HOME") {
        v.push(format!("{home}/.icons").into());
        v.push(format!("{home}/.local/share/icons").into());
    }
    v.push("/usr/share/icons".into());
    v.push("/usr/share/cursors/xorg-x11".into());
    v
}

/// The theme chain: $XCURSOR_THEME, then "default" — "default" on
/// most distros is a symlink into the configured system theme.
fn themes() -> Vec<String> {
    let mut t = Vec::new();
    if let Ok(th) = std::env::var("XCURSOR_THEME") {
        t.push(th);
    }
    t.push("default".into());
    t.push("Adwaita".into());
    t
}

/// Load a named cursor — walks theme×directory candidates and
/// returns the first parseable file, or None.
pub fn load(name: &str, want_size: u32) -> Option<CursorImg> {
    for dir in search_dirs() {
        for theme in themes() {
            let p = dir.join(&theme).join("cursors").join(name);
            if let Ok(bytes) = std::fs::read(&p) {
                if let Some(img) = parse(&bytes, want_size) {
                    return Some(img);
                }
            }
        }
    }
    None
}
