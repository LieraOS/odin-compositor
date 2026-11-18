// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_foreign_toplevel_list_v1 — the stabilized read-only
//! toplevel list (newer taskbars/window switchers use this
//! instead of zwlr_foreign_toplevel_manager_v1).
//!
//! Same lifecycle, different wire: every toplevel in `order`
//! gets one server-created handle per bound list — `toplevel`
//! announce, then title/app_id/identifier events closed out by
//! `done`. No control requests exist here by design — the ext
//! handle can only be destroyed, taskbar actions stay on the
//! wlr protocol.
//!
//! The lifecycle hooks mirror ft_new/ft_closed/ft_changed —
//! win/surface code calls into this module right next to the
//! wlr calls so both protocol families always agree.

use wayland_protocols::ext::foreign_toplevel_list::v1::server::{
    ext_foreign_toplevel_handle_v1 as fth,
    ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ext_foreign_toplevel_list_v1 as ftl, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1 as ExtForeignToplevelList;

/// One live ext handle — the link between a list client and
/// the toplevel it displays.
pub struct ExtFtLink {
    pub handle: ExtForeignToplevelHandleV1,
    pub surf: wl_surface::WlSurface,
}

impl CompState {
    /// A toplevel's stable identifier — unique per surface,
    /// constant for the session (spec asks for restart-stability;
    /// the object id is the honest identity we can give).
    fn ext_ftl_identifier(surf: &wl_surface::WlSurface) -> String {
        format!("odin-{}", surf.id())
    }

    /// Descriptor burst for one ext handle — title, app_id and
    /// identifier, closed by `done`. Used after creation and on
    /// every attribute change.
    fn ext_ftl_send(&self, link: &ExtFtLink) {
        let Some(sd) = link.surf.data::<SurfaceData>() else {
            return;
        };
        link.handle.title(sd.title.lock().unwrap().clone());
        link.handle.app_id(sd.app_id.lock().unwrap().clone());
        link.handle.identifier(Self::ext_ftl_identifier(&link.surf));
        link.handle.done();
    }

    /// Publish one toplevel to one list — server-created handle,
    /// `toplevel` event, then the descriptor.
    fn ext_ftl_new_for(&mut self, list: &ExtForeignToplevelListV1, surf: &wl_surface::WlSurface) {
        if !list.is_alive() {
            return;
        }
        let Some(client) = list.client() else {
            return;
        };
        let Ok(handle) = client
            .create_resource::<ExtForeignToplevelHandleV1, wl_surface::WlSurface, CompState>(
                &self.dh,
                list.version(),
                surf.clone(),
            )
        else {
            return;
        };
        list.toplevel(&handle);
        let link = ExtFtLink {
            handle,
            surf: surf.clone(),
        };
        self.ext_ftl_send(&link);
        self.ext_ftl_handles.push(link);
    }

    /// A toplevel joined the stacking order — every bound list
    /// learns about it through a fresh handle. Mirrors ft_new.
    pub(crate) fn ext_ftl_new(&mut self, surf: &wl_surface::WlSurface) {
        for list in self.ext_ftl_lists.clone() {
            self.ext_ftl_new_for(&list, surf);
        }
    }

    /// A list bound late — replay every live toplevel so the
    /// taskbar starts with the full window list. Mirrors
    /// ft_replay.
    pub(crate) fn ext_ftl_replay(&mut self, list: &ExtForeignToplevelListV1) {
        for surf in self.order.clone() {
            self.ext_ftl_new_for(list, &surf);
        }
    }

    /// A toplevel left the scene — `closed` on each of its ext
    /// handles, then drop them. Mirrors ft_closed.
    pub(crate) fn ext_ftl_closed(&mut self, surf: &wl_surface::WlSurface) {
        for l in self
            .ext_ftl_handles
            .iter()
            .filter(|l| l.surf.id() == surf.id())
        {
            if l.handle.is_alive() {
                l.handle.closed();
            }
        }
        self.ext_ftl_handles.retain(|l| l.surf.id() != surf.id());
    }

    /// Title/app_id moved — re-send the descriptor on every ext
    /// handle tracking this surface. Mirrors ft_changed.
    pub(crate) fn ext_ftl_changed(&mut self, surf: &wl_surface::WlSurface) {
        let links: Vec<ExtForeignToplevelHandleV1> = self
            .ext_ftl_handles
            .iter()
            .filter(|l| l.surf.id() == surf.id() && l.handle.is_alive())
            .map(|l| l.handle.clone())
            .collect();
        for handle in links {
            let link = ExtFtLink {
                handle,
                surf: surf.clone(),
            };
            self.ext_ftl_send(&link);
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        list: &ExtForeignToplevelListV1,
        request: ftl::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Client is done listening — finished() is the
            // destructor handshake, then drop it and its handles.
            ftl::Request::Stop => {
                list.finished();
                let cid = list.client().map(|c| c.id());
                state.ext_ftl_lists.retain(|l| l.id() != list.id());
                state
                    .ext_ftl_handles
                    .retain(|l| l.handle.client().map(|c| c.id()) != cid);
            }
            ftl::Request::Destroy => {
                let cid = list.client().map(|c| c.id());
                state.ext_ftl_lists.retain(|l| l.id() != list.id());
                state
                    .ext_ftl_handles
                    .retain(|l| l.handle.client().map(|c| c.id()) != cid);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        handle: &ExtForeignToplevelHandleV1,
        request: fth::Request,
        _: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Read-only by design — the only request is destroy.
        if let fth::Request::Destroy = request {
            state
                .ext_ftl_handles
                .retain(|l| l.handle.id() != handle.id());
        }
    }
}
