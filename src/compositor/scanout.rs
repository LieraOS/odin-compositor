// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client-buffer direct-scanout candidate detection — when the
//! whole scene on an output is a single buffer covering the
//! rect 1:1, the DRM backend can flip that buffer's dmabuf
//! straight to the CRTC instead of compositing it first.
//!
//! The cover rule for direct scanout: the TOPMOST item
//! intersecting the output must cover it completely — everything
//! completely — everything below is fully occluded, and any
//! item above it (popups, OSD layers, subsurfaces) fails the
//! cover test and falls back to compositing. Legacy page-flip
//! has no source offset or scaling, so the buffer must sit
//! exactly at the output origin at mode size.

use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::Resource;

use super::{BufInfo, CompState, SurfaceData};

impl CompState {
    /// The surface eligible for client-buffer direct scanout on
    /// the output rect `(ox, oy, w, h)` — `Some` only when the
    /// topmost scene item intersecting the rect covers it
    /// completely, shows an unscaled dmabuf at mode size, and
    /// draws fully opaque. A `None` answer means the composite
    /// path must run; it is the common case and costs nothing.
    pub fn direct_scan_candidate(&self, ox: i32, oy: i32, w: i32, h: i32) -> Option<WlSurface> {
        let top = self.scene().into_iter().rev().find(|it| {
            let (ix, iy) = it.pos();
            let (iw, ih) = it.extent();
            // Intersect test in global scene space — fills and
            // surfaces both occlude, so every item counts here.
            ix + iw > ox && iy + ih > oy && ix < ox + w && iy < oy + h
        })?;
        let super::render::SceneItem::Surf { surf, x, y } = &top else {
            return None; // a compositor fill on top means composite
        };
        let (ix, iy) = (*x, *y);
        let sd = surf.data::<SurfaceData>()?;
        // Covers means the buffer's top-left is exactly the
        // output origin — legacy flips scan the fb from (0,0),
        // so any offset would present a misaligned frame.
        let (bw, bh, has_dma, fmt) = {
            let shown = sd.shown_buf.lock().unwrap();
            let bi = shown.as_ref()?.data::<BufInfo>()?;
            (bi.w, bi.h, bi.dma.is_some(), bi.format)
        };
        // wl_shm fourcc values for XRGB8888/ARGB8888 — dmabuf
        // validation already restricts to these two, but the
        // check stays explicit for the shm-buffer corner case
        // (a plain shm fullscreen is not scannable anyway).
        const XR24: u32 = 0x34325258;
        const AR24: u32 = 0x34325241;
        if !has_dma || !matches!(fmt, XR24 | AR24) {
            return None;
        }
        // Viewport crop/scale and per-surface alpha change the
        // on-screen pixels — the CRTC would show the raw buffer,
        // so those surfaces must keep compositing.
        let opaque = sd.alpha.lock().unwrap().map(|a| a >= 0.999).unwrap_or(true);
        {
            let vp = sd.vp.lock().unwrap();
            if !opaque || vp.src.is_some() || vp.dst.is_some() {
                return None;
            }
        }
        // Exact-cover test: the buffer's top-left lands on the
        // output origin and the buffer is exactly mode-sized —
        // anything larger or offset needs crop/scaling the
        // legacy flip path cannot express.
        if ix == ox && iy == oy && bw == w && bh == h {
            Some(surf.clone())
        } else {
            None
        }
    }
}
