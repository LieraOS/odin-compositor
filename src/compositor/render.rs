// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Scene composite — blit all mapped surfaces into one output's
//! frame. `(ox, oy)` is the output's origin in global space;
//! surfaces are positioned globally and clipped to this viewport,
//! so each monitor renders exactly its slice of the scene.
//!
//! Order: toplevels bottom→top, then xdg popups (menus float above
//! every window), then the pointer image last — nothing may cover
//! the cursor.

use wayland_server::protocol::wl_surface;
use wayland_server::Resource;

use super::{input_method::ImData, text_input::TiData, BufInfo, CompState, SurfaceData};

impl CompState {
    /// Blit all mapped toplevel + popup buffers into `frame`.
    /// `with_cursor` toggles the pointer image — screencopy
    /// captures opt out of overlaying it.
    /// The full draw list in stacking order, shared by every
    /// render backend — layer bands below and above the window
    /// stack, popups, the session-lock gate, and the minimize /
    /// workspace visibility gates all live here so the CPU and
    /// GPU paths can't drift apart. The pointer is not part of
    /// the scene; backends draw it through their cursor path.
    ///
    /// `x`,`y` is where the buffer's top-left pixel lands in
    /// global scene coordinates — toplevels already have their
    /// CSD window-geometry margin and surface offset applied,
    /// popups carry parent position plus positioner offset.
    pub fn scene(&self) -> Vec<SceneItem> {
        // Shared placement math: stored position minus the CSD
        // margin plus the client attach offset — the formula
        // draw_surface used to apply inline.
        let place = |s: &wl_surface::WlSurface| -> Option<SceneItem> {
            let sd = s.data::<SurfaceData>()?;
            let pos = *sd.pos.lock().unwrap();
            let (gx, gy, _, _) = *sd.win_geom.lock().unwrap();
            let (sx, sy) = *sd.offset.lock().unwrap();
            Some(SceneItem::Surf {
                surf: s.clone(),
                x: pos.0 - gx + sx,
                y: pos.1 - gy + sy,
            })
        };
        // Session lock active: nothing but lock surfaces is
        // allowed on screen — windows, layers and popups all
        // skip their draw until unlock restores the scene.
        if self.session_locked() {
            return self.lock_surfaces().iter().filter_map(place).collect();
        }
        let mut out = Vec::new();
        // Layer bands below the window stack: wallpaper, then
        // bottom-layer elements (docks/panels that stay under).
        for l in self.layers.iter().filter(|l| {
            matches!(
                l.kind,
                super::layer::LayerKind::Background | super::layer::LayerKind::Bottom
            ) && l.configured
        }) {
            if let Some(it) = place(&l.surf) {
                self.push_with_subs(&mut out, it);
            }
        }
        for surf in &self.order {
            let Some(sd) = surf.data::<SurfaceData>() else {
                continue;
            };
            // Iconified windows hold their slot in the stacking
            // order — they just stop drawing until reactivated.
            if sd.minimized.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            // Windows on a hidden workspace draw nothing either —
            // same skip, different reason.
            if !self.ws_visible(surf) {
                continue;
            }
            // SSD chrome under the window's own pixels — the
            // frame rect covers the decorated extent so borders
            // and titlebar show around the content, the close
            // button sits inside the titlebar band.
            out.extend(self.ssd_fills(surf));
            if let Some(it) = place(surf) {
                self.push_with_subs(&mut out, it);
            }
        }
        // Popups ride above the whole stacking order — their draw
        // position follows the parent plus the positioner offset.
        for p in self.popups.iter().filter(|p| self.ws_visible(&p.parent)) {
            let Some(pp) = p
                .parent
                .data::<SurfaceData>()
                .map(|sd| *sd.pos.lock().unwrap())
            else {
                continue;
            };
            self.push_with_subs(
                &mut out,
                SceneItem::Surf {
                    surf: p.surf.clone(),
                    x: pp.0 + p.rel.0,
                    y: pp.1 + p.rel.1,
                },
            );
        }
        // X11 override-redirect windows (menus, tooltips) float
        // above the window stack like popups — `place` reads the
        // X11 geometry straight out of sd.pos.
        for s in &self.xpopups {
            if let Some(it) = place(s) {
                self.push_with_subs(&mut out, it);
            }
        }
        // Input-method popups (IME candidates, on-screen keyboard)
        // anchor at the focused field's cursor rect — the IM sent
        // the rect for the focused surface, so base = focus pos.
        let focus_base = self
            .kbd_focus
            .as_ref()
            .and_then(|f| f.data::<SurfaceData>())
            .map(|sd| *sd.pos.lock().unwrap())
            .unwrap_or((0, 0));
        let im_rect = self
            .im
            .as_ref()
            .and_then(|im| im.data::<ImData>())
            .and_then(|d| d.active_ti.lock().unwrap().clone())
            .and_then(|ti| ti.data::<TiData>().map(|d| *d.cursor_rect.lock().unwrap()))
            .unwrap_or((0, 0, 0, 0));
        for pop in self.im_popups.iter().filter(|p| p.obj.is_alive()) {
            self.push_with_subs(
                &mut out,
                SceneItem::Surf {
                    surf: pop.surf.clone(),
                    x: focus_base.0 + im_rect.0,
                    y: focus_base.1 + im_rect.1 + im_rect.3,
                },
            );
        }
        // Layer bands above everything but the cursor: top layers
        // (panels), then overlay (notifications, lock surfaces).
        for l in self.layers.iter().filter(|l| {
            matches!(
                l.kind,
                super::layer::LayerKind::Top | super::layer::LayerKind::Overlay
            ) && l.configured
        }) {
            if let Some(it) = place(&l.surf) {
                self.push_with_subs(&mut out, it);
            }
        }
        out
    }
}

/// Merge pairwise-overlapping damage rects until stable;
/// beyond 8 rects the whole set collapses into its bounding
/// box — keeps the per-blit clip list bounded.
pub fn merge_damage(rects: Vec<(i32, i32, i32, i32)>) -> Vec<(i32, i32, i32, i32)> {
    const MAX: usize = 8;
    let mut cur = rects;
    loop {
        let n = cur.len();
        let mut out: Vec<(i32, i32, i32, i32)> = Vec::new();
        'push: for r in cur.drain(..) {
            for m in out.iter_mut() {
                // Overlapping OR edge-adjacent rects merge —
                // adjacent halves of one surface stay one clip.
                if r.0 <= m.0 + m.2 && r.1 <= m.1 + m.3 && m.0 <= r.0 + r.2 && m.1 <= r.1 + r.3 {
                    let x = m.0.min(r.0);
                    let y = m.1.min(r.1);
                    m.2 = (m.0 + m.2).max(r.0 + r.2) - x;
                    m.3 = (m.1 + m.3).max(r.1 + r.3) - y;
                    m.0 = x;
                    m.1 = y;
                    continue 'push;
                }
            }
            out.push(r);
        }
        cur = out;
        if cur.len() >= n {
            break;
        }
    }
    if cur.len() > MAX {
        let x = cur.iter().map(|r| r.0).min().unwrap_or(0);
        let y = cur.iter().map(|r| r.1).min().unwrap_or(0);
        let r = cur.iter().map(|r| r.0 + r.2).max().unwrap_or(0);
        let b = cur.iter().map(|r| r.1 + r.3).max().unwrap_or(0);
        vec![(x, y, r - x, b - y)]
    } else {
        cur
    }
}

/// One drawable surface in the scene — what CompState::scene
/// returns. `x`,`y` is the buffer's top-left pixel in global
/// scene coordinates; backends only add their output offset.
/// One drawable slot in stacking order — either a client
/// surface at its global scene position, or a compositor-owned
/// solid rect. Fills draw SSD frames and any future chrome the
/// compositor paints itself; `rgba` is premultiplied BGRA in
/// frame-pixel order, matching every renderer's input layout.
pub enum SceneItem {
    Surf {
        surf: wayland_server::protocol::wl_surface::WlSurface,
        x: i32,
        y: i32,
    },
    Fill {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        rgba: [u8; 4],
    },
}

impl SceneItem {
    /// Top-left corner in global scene space — shared by clip
    /// tests in every consumer so fill and surface slots answer
    /// identically.
    pub fn pos(&self) -> (i32, i32) {
        match self {
            Self::Surf { x, y, .. } | Self::Fill { x, y, .. } => (*x, *y),
        }
    }

    /// On-screen extent in global space — fills carry their own
    /// size; surfaces report their shown buffer's dimensions.
    pub fn extent(&self) -> (i32, i32) {
        match self {
            Self::Fill { w, h, .. } => (*w, *h),
            Self::Surf { surf, .. } => surf
                .data::<SurfaceData>()
                .and_then(|sd| {
                    sd.shown_buf
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|b| b.data::<BufInfo>().map(|bi| (bi.w, bi.h)))
                })
                .unwrap_or((0, 0)),
        }
    }
}
