// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pointer hit-testing — which surface owns a global point.
//!
//! Hit order: popups first (menus float above windows), then
//! toplevels top→bottom, with input-region and lock-screen
//! gating. `hit_window` resolves popups to their parent so a
//! menu click focuses the owning window.

use wayland_server::protocol::wl_surface;
use wayland_server::Resource;

use super::{CompState, SurfaceData};

impl CompState {
    /// Surface-local hit test: topmost popup first (menus float
    /// above windows), then toplevels in stacking order. Returns
    /// the surface plus the local coordinates of the hit.
    /// Is surface-local point (lx, ly) inside the surface's
    /// input region? None = whole surface (protocol default),
    /// Some(vec![]) = deliberately input-transparent — the
    /// X11 XShape input mask lands here via XWayland, so a
    /// transparent overlay must not eat clicks.
    pub(crate) fn in_region(sd: &SurfaceData, lx: f64, ly: f64) -> bool {
        match &*sd.input_region.lock().unwrap() {
            None => true,
            Some(rects) => rects.iter().any(|&(rx, ry, rw, rh)| {
                lx >= rx as f64 && lx < (rx + rw) as f64 && ly >= ry as f64 && ly < (ry + rh) as f64
            }),
        }
    }

    pub(crate) fn hit(&self, x: f64, y: f64) -> Option<(wl_surface::WlSurface, f64, f64)> {
        // Session locked — the lock surfaces are the only
        // legal pointer targets in the whole scene.
        if self.session_locked() {
            return self.hit_lock(x, y);
        }
        // X11 override-redirect surfaces sit on top of the whole
        // scene — menus must be hit before any xdg popup below.
        for s in self.xpopups.iter().rev() {
            if !self.input_allowed(s.client().as_ref()) {
                continue;
            }
            let Some(sd) = s.data::<SurfaceData>() else {
                continue;
            };
            let (px, py) = *sd.pos.lock().unwrap();
            let (w, h) = self.surf_size(sd);
            if x >= px as f64
                && x < (px + w) as f64
                && y >= py as f64
                && y < (py + h) as f64
                && Self::in_region(sd, x - px as f64, y - py as f64)
            {
                return Some((s.clone(), x - px as f64, y - py as f64));
            }
        }
        for p in self.popups.iter().rev() {
            // A held input inhibitor turns foreign surfaces into
            // input-ghosts — they must not even be found here.
            if !self.input_allowed(p.surf.client().as_ref()) {
                continue;
            }
            // Popups of a hidden-workspace parent can't be hit.
            if !self.ws_visible(&p.parent) {
                continue;
            }
            let parent_pos = p
                .parent
                .data::<SurfaceData>()
                .map(|sd| *sd.pos.lock().unwrap())
                .unwrap_or((0, 0));
            let px = parent_pos.0 + p.rel.0;
            let py = parent_pos.1 + p.rel.1;
            if x >= px as f64
                && x < (px + p.size.0) as f64
                && y >= py as f64
                && y < (py + p.size.1) as f64
                && p.surf
                    .data::<SurfaceData>()
                    .is_some_and(|sd| Self::in_region(sd, x - px as f64, y - py as f64))
            {
                return Some((p.surf.clone(), x - px as f64, y - py as f64));
            }
        }
        // Layers above toplevels hit first — notifications and
        // panels sit over windows by definition.
        if let Some(r) = self.hit_layer(
            x,
            y,
            &[
                super::layer::LayerKind::Overlay,
                super::layer::LayerKind::Top,
            ],
        ) {
            return Some(r);
        }
        for s in self.order.iter().rev() {
            // Hidden-workspace windows are click-through ghosts.
            if !self.ws_visible(s) {
                continue;
            }
            if !self.input_allowed(s.client().as_ref()) {
                continue;
            }
            let Some(sd) = s.data::<SurfaceData>() else {
                continue;
            };
            if sd.is_cursor.load(std::sync::atomic::Ordering::SeqCst)
                || sd.minimized.load(std::sync::atomic::Ordering::SeqCst)
            {
                continue;
            }
            let (px, py) = *sd.pos.lock().unwrap();
            let (gx, gy, _, _) = *sd.win_geom.lock().unwrap();
            // Hit-test the surface's effective rect — the client
            // owns the interactive boundary via set_input_region
            // (default: whole buffer INCLUDING the shadow margin,
            // which is exactly where CSD resize grips live).
            let (bw, bh) = self.surf_size(sd);
            let wx = px - gx;
            let wy = py - gy;
            // Subsurfaces can poke outside the parent's rect —
            // above-band children hit before the parent's own
            // pixels, below-band children after it.
            if let Some(r) = self.hit_subs(s, wx, wy, x, y, true) {
                return Some(r);
            }
            if x >= wx as f64
                && x < (wx + bw) as f64
                && y >= wy as f64
                && y < (wy + bh) as f64
                && Self::in_region(sd, x - wx as f64, y - wy as f64)
            {
                return Some((s.clone(), x - wx as f64, y - wy as f64));
            }
            if let Some(r) = self.hit_subs(s, wx, wy, x, y, false) {
                return Some(r);
            }
        }
        // Background and bottom layers are the last resort —
        // clicking the wallpaper still counts as a surface hit.
        self.hit_layer(
            x,
            y,
            &[
                super::layer::LayerKind::Bottom,
                super::layer::LayerKind::Background,
            ],
        )
    }

    /// The window under a point for compositor purposes (focus,
    /// raise) — popups return their parent so a menu click keeps
    /// the owning window focused.
    pub(crate) fn hit_window(&self, x: f64, y: f64) -> Option<(wl_surface::WlSurface, f64, f64)> {
        if let Some((s, lx, ly)) = self.hit(x, y) {
            if let Some(pop) = self.popups.iter().find(|p| p.surf.id() == s.id()) {
                let pos = pop
                    .parent
                    .data::<SurfaceData>()
                    .map(|sd| *sd.pos.lock().unwrap())
                    .unwrap_or((0, 0));
                return Some((pop.parent.clone(), x - pos.0 as f64, y - pos.1 as f64));
            }
            return Some((s, lx, ly));
        }
        None
    }

    /// Pointer focus evaluation — enter/leave fire only on an
    /// actual change, and only to the surface-owning client's
    /// objects (foreign ids in enter are a protocol error).    /// A surface's top-left in global space for coordinate
    /// translation — popups derive it from parent + rel offset.
    pub(crate) fn local_origin(&self, surf: &wl_surface::WlSurface) -> (f64, f64) {
        // Subsurfaces resolve through their parent chain — a
        // child's origin is the parent's buffer origin plus the
        // applied set_position offset.
        if let Some(sd) = surf.data::<SurfaceData>() {
            if let Some(p) = sd.sub_parent.lock().unwrap().clone() {
                if p.is_alive() {
                    let (ox, oy) = *sd.sub_pos.lock().unwrap();
                    let (px, py) = self.local_origin(&p);
                    return (px + ox as f64, py + oy as f64);
                }
            }
        }
        if let Some(pop) = self.popups.iter().find(|p| p.surf.id() == surf.id()) {
            let pos = pop
                .parent
                .data::<SurfaceData>()
                .map(|sd| *sd.pos.lock().unwrap())
                .unwrap_or((0, 0));
            return ((pos.0 + pop.rel.0) as f64, (pos.1 + pop.rel.1) as f64);
        }
        surf.data::<SurfaceData>()
            .map(|sd| {
                let (px, py) = *sd.pos.lock().unwrap();
                let (gx, gy, _, _) = *sd.win_geom.lock().unwrap();
                ((px - gx) as f64, (py - gy) as f64)
            })
            .unwrap_or((0.0, 0.0))
    }
}
