// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_foreign_toplevel_management_v1 — taskbars and window
//! lists.
//!
//! Each bound manager client gets one handle object per mapped
//! toplevel. Handles carry title/app_id/output/state and accept
//! window ops (activate, close, maximize, minimize, fullscreen)
//! so a dock or switcher can drive the session without being a
//! client of the windows it manages.
//!
//! Minimize is real here: an iconified window stays in the
//! stacking order but leaves rendering and hit-testing until
//! reactivated — exactly what a taskbar expects.

use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1 as fth,
    zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    zwlr_foreign_toplevel_manager_v1 as ftm,
    zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

// Re-export — mod.rs names the global through this path.
pub use wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 as ZwlrForeignToplevelManager;

/// Handle-state enum ids — the u32 values the `state` array
/// carries (maximized=0, minimized=1, activated=2,
/// fullscreen=3).
const ST_MAXIMIZED: u32 = 0;
const ST_MINIMIZED: u32 = 1;
const ST_ACTIVATED: u32 = 2;
const ST_FULLSCREEN: u32 = 3;

/// One live handle object — the link between a manager client
/// and the toplevel it displays.
pub struct FtLink {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub surf: wl_surface::WlSurface,
}

impl CompState {
    /// State byte array for one toplevel — mirrors tl_states
    /// plus our minimize flag and the live keyboard focus.
    fn ft_state_of(&self, surf: &wl_surface::WlSurface) -> Vec<u8> {
        let mut ids: Vec<u32> = Vec::new();
        if let Some(sd) = surf.data::<SurfaceData>() {
            for st in sd.tl_states.lock().unwrap().iter() {
                match st {
                    xdg_toplevel::State::Maximized => ids.push(ST_MAXIMIZED),
                    xdg_toplevel::State::Fullscreen => ids.push(ST_FULLSCREEN),
                    _ => {}
                }
            }
            if sd.minimized.load(std::sync::atomic::Ordering::SeqCst) {
                ids.push(ST_MINIMIZED);
            }
        }
        if self
            .kbd_focus
            .as_ref()
            .map(|f| f.id() == surf.id())
            .unwrap_or(false)
        {
            ids.push(ST_ACTIVATED);
        }
        ids.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// The wl_output resource owned by this manager's client —
    /// output_enter/leave must name a resource that client
    /// actually bound.
    fn ft_output_for(
        &self,
        mgr: &ZwlrForeignToplevelManagerV1,
    ) -> Option<wayland_server::protocol::wl_output::WlOutput> {
        let cid = mgr.client().map(|c| c.id());
        self.outputs
            .iter()
            .find(|(o, _)| o.client().map(|c| c.id()) == cid)
            .map(|(o, _)| o.clone())
    }

    /// Send one handle's full descriptor — used after creation
    /// and after any title/app_id/state change.
    fn ft_send(&self, link: &FtLink) {
        let Some(sd) = link.surf.data::<SurfaceData>() else {
            return;
        };
        link.handle.title(sd.title.lock().unwrap().clone());
        link.handle.app_id(sd.app_id.lock().unwrap().clone());
        if let Some(mgr) = self
            .ft_managers
            .iter()
            .find(|m| m.client().map(|c| c.id()) == link.handle.client().map(|c| c.id()))
            .cloned()
        {
            if let Some(out) = self.ft_output_for(&mgr) {
                link.handle.output_enter(&out);
            }
        }
        link.handle.state(self.ft_state_of(&link.surf));
        link.handle.done();
    }

    /// Publish one toplevel to one manager — server-created
    /// handle object, `toplevel` event, then the descriptor.
    fn ft_new_for(&mut self, mgr: &ZwlrForeignToplevelManagerV1, surf: &wl_surface::WlSurface) {
        if !mgr.is_alive() {
            return;
        }
        let Some(client) = mgr.client() else { return };
        let Ok(handle) = client
            .create_resource::<ZwlrForeignToplevelHandleV1, wl_surface::WlSurface, CompState>(
                &self.dh,
                mgr.version().min(3),
                surf.clone(),
            )
        else {
            return;
        };
        mgr.toplevel(&handle);
        let link = FtLink {
            handle,
            surf: surf.clone(),
        };
        self.ft_send(&link);
        self.ft_handles.push(link);
    }

    /// A toplevel joined the stacking order — every bound
    /// manager learns about it through a fresh handle. The ext
    /// list sees the same event on its own channel.
    pub(crate) fn ft_new(&mut self, surf: &wl_surface::WlSurface) {
        for mgr in self.ft_managers.clone() {
            self.ft_new_for(&mgr, surf);
        }
        self.ext_ftl_new(surf);
    }

    /// A manager bound late — replay every live toplevel so the
    /// taskbar starts with the full window list, not just with
    /// whatever maps next.
    pub(crate) fn ft_replay(&mut self, mgr: &ZwlrForeignToplevelManagerV1) {
        for surf in self.order.clone() {
            self.ft_new_for(mgr, &surf);
        }
    }

    /// A toplevel left the scene — `closed` on each of its
    /// handles, then drop them.
    pub(crate) fn ft_closed(&mut self, surf: &wl_surface::WlSurface) {
        for l in self.ft_handles.iter().filter(|l| l.surf.id() == surf.id()) {
            if l.handle.is_alive() {
                l.handle.closed();
            }
        }
        self.ft_handles.retain(|l| l.surf.id() != surf.id());
        self.ext_ftl_closed(surf);
        // Toplevel capture sessions lose their source too.
        self.cap_src_died(&crate::compositor::copy_capture::CapSrc::Toplevel(
            surf.clone(),
        ));
    }

    /// Title/app_id/state moved — re-send the descriptor on
    /// every handle tracking this surface.
    pub(crate) fn ft_changed(&mut self, surf: &wl_surface::WlSurface) {
        for l in self.ft_handles.iter().filter(|l| l.surf.id() == surf.id()) {
            if l.handle.is_alive() {
                self.ft_send(l);
            }
        }
        self.ext_ftl_changed(surf);
    }

    /// Focus moved — only the activated bit changes, on every
    /// handle. Cheap enough to refresh wholesale.
    pub(crate) fn ft_refresh_focus(&self) {
        for l in &self.ft_handles {
            if l.handle.is_alive() {
                l.handle.state(self.ft_state_of(&l.surf));
                l.handle.done();
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        mgr: &ZwlrForeignToplevelManagerV1,
        request: ftm::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Client is done listening — finished() is the
            // destructor handshake, then drop it and its handles.
            ftm::Request::Stop => {
                mgr.finished();
                let cid = mgr.client().map(|c| c.id());
                state.ft_managers.retain(|m| m.id() != mgr.id());
                state
                    .ft_handles
                    .retain(|l| l.handle.client().map(|c| c.id()) != cid);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: fth::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let surf = surf.clone();
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        match request {
            fth::Request::Activate { .. } => {
                // Same as a click on the window: raise + keyboard
                // focus; the Activated state mirrors out below.
                state.raise(&surf);
                state.set_kbd_focus(Some(surf.clone()));
            }
            fth::Request::Close => {
                if let Some(tl) = sd.xdg_tl.lock().unwrap().as_ref() {
                    tl.close();
                }
            }
            fth::Request::SetMaximized => {
                state.set_maximized_state(sd, true);
            }
            fth::Request::UnsetMaximized => {
                state.set_maximized_state(sd, false);
            }
            fth::Request::SetMinimized => {
                sd.minimized
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                state.damage_everything();
            }
            fth::Request::UnsetMinimized => {
                sd.minimized
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                state.damage_everything();
            }
            fth::Request::SetFullscreen { .. } => {
                state.set_fullscreen_state(sd, true);
            }
            fth::Request::UnsetFullscreen => {
                state.set_fullscreen_state(sd, false);
            }
            fth::Request::SetRectangle { .. } => {
                // Window-preview rectangle hint — docks use it to
                // place thumbnails; there's nothing to do with it
                // on the compositor side.
            }
            fth::Request::Destroy => {
                state.ft_handles.retain(|l| l.handle.id() != handle.id());
            }
            _ => {}
        }
        // Whatever the request did, the descriptor may have
        // changed — re-send title/app_id/state to this handle.
        if handle.is_alive() {
            let link = FtLink {
                handle: handle.clone(),
                surf,
            };
            state.ft_send(&link);
        }
    }
}
