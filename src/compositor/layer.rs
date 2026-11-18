// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_layer_shell_v1 — panels, wallpapers, launchers,
//! notifications.
//!
//! Layer surfaces are NOT windows: they never enter the stacking
//! `order`, never cascade, and position themselves from
//! anchor+margin+exclusive-zone rules against their output.
//! Render order bottom→top: background layers, bottom layers,
//! toplevels, popups, top layers, overlay layers.
//!
//! Exclusive zones shrink the usable rect — a panel claiming 24px
//! at the top edge pushes every toplevel's tile down by that
//! amount, so windows never hide behind the panel.

use tracing::info;
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_surface_v1 as lsf, zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::Resource;

use super::{BufInfo, CompState, SurfaceData};

// Re-export — mod.rs names the global through this path.
pub use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::ZwlrLayerShellV1 as ZwlrLayerShell;

/// The four stacking layers, in draw order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// One live layer surface — anchors against an output edge or
/// fills it, optionally reserving an exclusive zone.
pub struct Layer {
    pub surf: wl_surface::WlSurface,
    pub obj: ZwlrLayerSurfaceV1,
    pub kind: LayerKind,
    /// Anchor bitfield — edge values top=1 bottom=2 left=4
    /// right=8, matching the protocol's anchor enum.
    pub anchor: u32,
    /// Exclusive zone in pixels (>=0), or -1 = center ignoring
    /// other layers' zones.
    pub exclusive: i32,
    /// Margins (top, right, bottom, left) around the anchor.
    pub margin: (i32, i32, i32, i32),
    /// Client-requested size — 0 on an anchored axis means the
    /// compositor fills the usable span.
    pub size: (u32, u32),
    /// Keyboard interactivity: 0 none, 1 exclusive, 2 on_demand.
    pub kbd: u32,
    /// Configure serials awaiting the client's ack — spec
    /// requires every configure to round-trip once.
    pub pending: Vec<u32>,
    /// True once the client acked at least one configure — only
    /// then may the first commit map the surface.
    pub configured: bool,
    /// Explicit exclusive edge from set_exclusive_edge (v5) —
    /// 0 = derive from the anchor bits instead.
    pub excl_edge: u32,
    /// Placed rect in global space after the last layout pass —
    /// exclusive-zone math and hit-testing read it back.
    pub rect: (i32, i32, i32, i32),
    /// Which output this layer anchors to — the wl_output the
    /// client passed, or None = primary. Stored per spec; the
    /// placement path consults it once outputs get real routing.
    #[allow(dead_code)]
    pub output: Option<wayland_server::protocol::wl_output::WlOutput>,
}

/// Anchor enum → bitfield — shared with the dispatch glue in
/// `layer_dispatch.rs`.
pub(crate) fn edge(a: lsf::Anchor) -> u32 {
    match a {
        lsf::Anchor::Top => 1,
        lsf::Anchor::Bottom => 2,
        lsf::Anchor::Left => 4,
        lsf::Anchor::Right => 8,
        _ => 0,
    }
}

impl CompState {
    /// Recompute the configure a layer surface should see:
    /// dimensions the client left at 0 on anchored axes are
    /// filled with the usable span — a wallpaper anchored on all
    /// four edges with size 0×0 gets the whole output, a top
    /// panel with size (0,24) gets (usable_width,24). Every
    /// configure carries a serial the client must ack.
    pub(crate) fn reconfigure_layer(&mut self, i: usize) {
        let l = &self.layers[i];
        let (rx, ry, rw, rh) = self.layer_output_rect(i);
        let _ = (rx, ry);
        let (mt, mr, mb, ml) = l.margin;
        let h_span = l.anchor & 4 != 0 && l.anchor & 8 != 0;
        let v_span = l.anchor & 1 != 0 && l.anchor & 2 != 0;
        let w = if h_span || l.size.0 == 0 && l.anchor & (4 | 8) != 0 {
            (rw as i32 - ml - mr).max(0) as u32
        } else {
            l.size.0
        };
        let h = if v_span || l.size.1 == 0 && l.anchor & (1 | 2) != 0 {
            (rh as i32 - mt - mb).max(0) as u32
        } else {
            l.size.1
        };
        let serial = self.next_serial_pub();
        let l = &mut self.layers[i];
        l.pending.push(serial);
        l.obj.configure(serial, w, h);
    }

    /// Find the layer record for a wl_surface — hit-testing and
    /// the commit path both key on the surface id.
    pub(crate) fn layer_index(&self, surf: &wl_surface::WlSurface) -> Option<usize> {
        self.layers.iter().position(|l| l.surf.id() == surf.id())
    }

    /// The output a layer anchors to — its bound wl_output maps
    /// to our single advertised output today; absent → primary.
    fn layer_output_rect(&self, i: usize) -> (i32, i32, u32, u32) {
        // wl_output binding isn't per-physical-output yet — every
        // layer resolves to the primary rect for now.
        let _ = i;
        self.layout.primary_rect()
    }

    /// Usable rect of one output after the exclusive zones of the
    /// layers anchored to it — tiling and the float cascade live
    /// inside it so windows never slide under a panel. Layer
    //  surfaces resolve to the primary output today, so only the
    //  primary's rect shrinks; other outputs return their full
    //  mode rect.
    pub(crate) fn usable_rect_of(&self, name: &str) -> (i32, i32, u32, u32) {
        let Some(out) = self.layout.outputs.get(name) else {
            return self.layout.primary_rect();
        };
        let (mut x, mut y, mut w, mut h) = (out.x, out.y, out.mode.0, out.mode.1);
        if !out.primary {
            return (x, y, w, h);
        }
        for l in &self.layers {
            if !l.configured || l.exclusive <= 0 {
                continue;
            }
            let (lx, ly, lw, lh) = l.rect;
            let z = l.exclusive;
            // The zone eats into whichever edge the layer hugs —
            // an explicit exclusive_edge wins over anchor-derived.
            let e = if l.excl_edge != 0 {
                l.excl_edge
            } else {
                l.anchor
            };
            if e & 1 != 0 && ly <= y {
                let z = z.min(h as i32);
                y += z;
                h = h.saturating_sub(z as u32);
            } else if e & 2 != 0 {
                h = h.saturating_sub(z as u32);
            } else if e & 4 != 0 && lx <= x {
                let z = z.min(w as i32);
                x += z;
                w = w.saturating_sub(z as u32);
            } else if e & 8 != 0 {
                w = w.saturating_sub(z as u32);
            }
            let _ = (lx, lw, lh);
        }
        (x, y, w, h)
    }

    /// Usable rect of the primary output — the float cascade and
    /// other primary-anchored callers keep this spelling.
    pub(crate) fn usable_rect(&self) -> (i32, i32, u32, u32) {
        let name = self
            .layout
            .outputs
            .values()
            .find(|o| o.primary)
            .map(|o| o.name.clone())
            .unwrap_or_default();
        self.usable_rect_of(&name)
    }

    /// Compute a layer surface's rect from anchor/margin/size
    /// inside its usable span — zones of EARLIER mapped layers
    /// already shrink the span, so later panels stack correctly.
    fn place_layer(&mut self, i: usize) {
        // Usable span before THIS layer's own zone.
        let saved = self.layers[i].exclusive;
        self.layers[i].exclusive = 0;
        let (rx, ry, rw, rh) = self.usable_rect();
        self.layers[i].exclusive = saved;

        let l = &self.layers[i];
        let (mt, mr, mb, ml) = l.margin;
        let (mut w, mut h) = (l.size.0 as i32, l.size.1 as i32);
        if l.anchor & 4 != 0 && l.anchor & 8 != 0 || w == 0 {
            w = rw as i32 - ml - mr;
        }
        if l.anchor & 1 != 0 && l.anchor & 2 != 0 || h == 0 {
            h = rh as i32 - mt - mb;
        }
        let x = if l.anchor & 4 != 0 {
            rx + ml
        } else if l.anchor & 8 != 0 {
            rx + rw as i32 - mr - w
        } else {
            rx + (rw as i32 - w) / 2
        };
        let y = if l.anchor & 1 != 0 {
            ry + mt
        } else if l.anchor & 2 != 0 {
            ry + rh as i32 - mb - h
        } else {
            ry + (rh as i32 - h) / 2
        };
        let rect = (x, y, w.max(0), h.max(0));
        self.layers[i].rect = rect;
        if let Some(sd) = self.layers[i].surf.data::<SurfaceData>() {
            *sd.pos.lock().unwrap() = (rect.0, rect.1);
        }
    }

    /// Commit-time hook for layer surfaces — called from
    /// surface.rs instead of the toplevel map path. The first
    /// configured+committed buffer maps the surface, places it
    /// and re-lays the workspace (exclusive zones shift tiles).
    pub(crate) fn commit_layer(&mut self, surf: &wl_surface::WlSurface) {
        let Some(i) = self.layer_index(surf) else {
            return;
        };
        if !self.layers[i].configured {
            return; // still waiting on the ack_configure round-trip
        }
        let was_mapped = self.layers[i]
            .surf
            .data::<SurfaceData>()
            .map(|sd| sd.mapped.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false);
        // Remember the footprint before placement so a geometry
        // change while mapped can repaint just the touched
        // rects — content damage arrives via the surface
        // damage path, not here.
        let old_rect = self.layers[i].rect;
        self.place_layer(i);
        if let Some(sd) = surf.data::<SurfaceData>() {
            let dims = sd
                .shown_buf
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|b| b.data::<BufInfo>())
                .map(|bi| (bi.w, bi.h));
            tracing::info!(dims = ?dims, "layer commit: shown buffer");
        }
        if !was_mapped {
            if let Some(sd) = surf.data::<SurfaceData>() {
                sd.mapped.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            info!(layer = ?self.layers[i].kind, rect = ?self.layers[i].rect, "layer surface mapped");
            // Exclusive-interactive layers (launchers, lock
            // surfaces) take keyboard focus on map like a grab.
            if self.layers[i].kbd == 1 {
                self.set_kbd_focus(Some(surf.clone()));
            }
            // Work area changed — tiled windows must slide out
            // from under the new panel.
            self.apply_layout();
            self.damage_everything();
        } else {
            let rect = self.layers[i].rect;
            if rect != old_rect {
                // Geometry moved while mapped — repaint both
                // footprints. A content-only commit adds no
                // extra damage; the surface path covers it.
                self.damage.push(old_rect);
                self.damage.push(rect);
            }
        }
    }

    /// Layer surfaces above a point — hit-testing walks these
    /// around the toplevel stack: overlay & top first, bottom &
    /// background last.
    pub(crate) fn hit_layer(
        &self,
        x: f64,
        y: f64,
        bands: &[LayerKind],
    ) -> Option<(wl_surface::WlSurface, f64, f64)> {
        for l in self.layers.iter().rev() {
            if !bands.contains(&l.kind) || !l.configured {
                continue;
            }
            if !self.input_allowed(l.surf.client().as_ref()) {
                continue;
            }
            let Some(sd) = l.surf.data::<SurfaceData>() else {
                continue;
            };
            if !sd.mapped.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            let (bw, bh) = sd
                .shown_buf
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|b| b.data::<BufInfo>())
                .map(|bi| (bi.w, bi.h))
                .unwrap_or((0, 0));
            let (px, py) = *sd.pos.lock().unwrap();
            if x >= px as f64
                && x < (px + bw) as f64
                && y >= py as f64
                && y < (py + bh) as f64
                && Self::in_region(sd, x - px as f64, y - py as f64)
            {
                return Some((l.surf.clone(), x - px as f64, y - py as f64));
            }
        }
        None
    }

    /// Does a layer surface accept keyboard focus on click —
    /// on_demand and exclusive do, none never does.
    pub(crate) fn layer_wants_kbd(&self, surf: &wl_surface::WlSurface) -> bool {
        self.layers
            .iter()
            .find(|l| l.surf.id() == surf.id())
            .map(|l| l.kbd != 0)
            .unwrap_or(false)
    }

    /// wl_surface destroy hook for layers — drop the record and
    /// hand the freed exclusive zone back to the workspace.
    pub(crate) fn remove_layer(&mut self, surf: &wl_surface::WlSurface) {
        if self.layer_index(surf).is_some() {
            self.layers.retain(|l| l.surf.id() != surf.id());
            self.damage_everything();
            self.apply_layout();
        }
    }
}
