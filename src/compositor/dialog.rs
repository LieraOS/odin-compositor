// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `xdg_dialog_v1` — marks a toplevel as a dialog and carries
//! the modal flag. Modal dialogs are transient-for their parent
//! (same-client `set_parent` or `xdg_foreign_v2`), so they
//! already restack above it; the committed flag on SurfaceData
//! is what focus policy reads to keep input on the dialog.

use wayland_protocols::xdg::dialog::v1::server::{
    xdg_dialog_v1 as d, xdg_dialog_v1::XdgDialogV1, xdg_wm_dialog_v1 as wd,
    xdg_wm_dialog_v1::XdgWmDialogV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::xdg::dialog::v1::server::xdg_wm_dialog_v1::XdgWmDialogV1 as XdgWmDialog;

impl Dispatch<XdgWmDialogV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgWmDialogV1,
        request: wd::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wd::Request::GetXdgDialog { id, toplevel } => {
                // The xdg_toplevel's object data is its owning
                // wl_surface — same convention as xdg_dispatch.
                if let Some(surf) = toplevel.data::<wl_surface::WlSurface>() {
                    init.init::<XdgDialogV1, wl_surface::WlSurface>(id, surf.clone());
                }
            }
            wd::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgDialogV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgDialogV1,
        request: d::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(sd) = surface.data::<SurfaceData>() else {
            return;
        };
        match request {
            d::Request::SetModal => {
                *sd.modal.lock().unwrap() = true;
            }
            d::Request::UnsetModal => {
                *sd.modal.lock().unwrap() = false;
            }
            d::Request::Destroy => {}
            _ => {}
        }
    }
}

impl CompState {
    /// Topmost mapped modal dialog in `surf`'s transient subtree —
    /// xdg_dialog_v1 semantics make a modal dialog block input to
    /// its ancestors, so focus requests for an ancestor redirect
    /// to the modal child. Walks `order` so the topmost wins.
    pub(crate) fn modal_descendant(
        &self,
        surf: &wl_surface::WlSurface,
    ) -> Option<wl_surface::WlSurface> {
        let mut top: Option<wl_surface::WlSurface> = None;
        for s in &self.order {
            let Some(sd) = s.data::<SurfaceData>() else {
                continue;
            };
            if !*sd.modal.lock().unwrap() {
                continue;
            }
            // Climb the transient chain — reaching `surf` makes s
            // a modal descendant of it.
            let mut cur = sd.transient_for.lock().unwrap().clone();
            while let Some(p) = cur {
                if p.id() == surf.id() {
                    top = Some(s.clone());
                    break;
                }
                cur = p
                    .data::<SurfaceData>()
                    .and_then(|pd| pd.transient_for.lock().unwrap().clone());
            }
        }
        top
    }
}
