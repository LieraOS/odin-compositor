// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X11 override-redirect windows — menus, tooltips and
//! dropdowns that ICCCM places outside WM management: they
//! keep their X11 geometry, never tile, never join the
//! stacking order and draw above all windows like popups.

use wayland_server::Resource;

use super::{CompState, SurfaceData};
use wayland_server::protocol::wl_surface;

impl CompState {
    /// True when this wl_surface belongs to an X window created
    /// with override-redirect — the flag arrives via CreateNotify
    /// before the WL_SURFACE_ID association binds.
    pub(crate) fn xwin_or(&self, surf: &wl_surface::WlSurface) -> bool {
        surf.data::<SurfaceData>()
            .and_then(|sd| *sd.xwin.lock().unwrap())
            .and_then(|wid| self.xwins.get(&wid).map(|x| x.override_redirect))
            .unwrap_or(false)
    }

    /// Put an override-redirect surface on screen at its X11
    /// geometry — `sd.pos` is the global position synced from
    /// ConfigureRequest/MapNotify, `surf_size` the live dims.
    /// Idempotent: a re-committed OR surface keeps its entry.
    pub(crate) fn map_xpopup(&mut self, surf: &wl_surface::WlSurface) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        sd.mapped.store(true, std::sync::atomic::Ordering::SeqCst);
        if !self.xpopups.iter().any(|s| s.id() == surf.id()) {
            self.xpopups.push(surf.clone());
            let (x, y, w, h) = *sd.win_geom.lock().unwrap();
            let pos = *sd.pos.lock().unwrap();
            tracing::info!(?pos, x, y, w, h, "xwm: override-redirect on screen");
        }
        self.damage_everything();
    }

    /// Take an override-redirect surface off screen — called on
    /// UnmapNotify, DestroyNotify and surface death. Drops
    /// pointer/keyboard focus when it was the focused surface.
    pub(crate) fn xpopup_remove(&mut self, surf: &wl_surface::WlSurface) {
        if !self.xpopups.iter().any(|s| s.id() == surf.id()) {
            return;
        }
        self.xpopups.retain(|s| s.id() != surf.id());
        if self.focus.as_ref().is_some_and(|f| f.id() == surf.id()) {
            self.focus = None;
        }
        if self.kbd_focus.as_ref().is_some_and(|f| f.id() == surf.id()) {
            self.set_kbd_focus(None);
        }
        self.damage_everything();
    }
}
