// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_primary_selection_v1 — the middle-click paste channel.
//!
//! Same shape as wl_data_device but for PRIMARY: owners create
//! a source, advertise mimes via `offer` and call
//! `set_selection`; the keyboard-focused client's devices then
//! get `data_offer` + `selection`. Pastes route
//! `offer.receive(mime, fd)` straight into `source.send` — the
//! owner writes directly into the reader's pipe.
//!
//! The selection itself is the unified `SelSrc` — the same
//! slot data-control devices observe via `primary_selection`,
//! so a clipboard-manager primary and a wp-primary are one
//! owner, not two channels that could disagree.

use std::os::fd::AsFd;
use tracing::info;
use wayland_protocols::wp::primary_selection::zv1::server::{
    zwp_primary_selection_device_manager_v1 as psm, zwp_primary_selection_device_v1 as psd,
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_offer_v1 as pso,
    zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    zwp_primary_selection_source_v1 as pss,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::data::{SelSrc, SourceData};
use super::CompState;

/// Global type re-export — globals.rs registers it under the
/// module path like every other protocol here.
pub use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;

impl GlobalDispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for CompState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpPrimarySelectionDeviceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init::<ZwpPrimarySelectionDeviceManagerV1, ()>(resource, ());
        info!("primary selection manager bound");
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionDeviceManagerV1,
        request: psm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            psm::Request::CreateSource { id } => {
                init.init::<ZwpPrimarySelectionSourceV1, SourceData>(id, SourceData::default());
            }
            psm::Request::GetDevice { id, .. } => {
                let dev = init.init::<ZwpPrimarySelectionDeviceV1, ()>(id, ());
                state.prim_devices.push(dev);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, SourceData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        src: &ZwpPrimarySelectionSourceV1,
        request: pss::Request,
        data: &SourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            pss::Request::Offer { mime_type } => {
                data.mimes.lock().unwrap().push(mime_type);
            }
            pss::Request::Destroy => {
                // Owner's source died while holding primary —
                // publish the empty selection like a deselect.
                if matches!(&state.primary, Some(SelSrc::Prim(s)) if s == src) {
                    state.set_primary(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionDeviceV1,
        request: psd::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // The serial is advisory (which event justified the
            // set) — accepting it unconditionally matches what
            // wl_data_device already does.
            psd::Request::SetSelection { source, .. } => {
                info!("primary selection set");
                state.set_primary(source.map(SelSrc::Prim));
            }
            psd::Request::Destroy => {
                state.prim_devices.retain(|d| d.is_alive());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionOfferV1, SelSrc> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionOfferV1,
        request: pso::Request,
        data: &SelSrc,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let pso::Request::Receive { mime_type, fd } = request {
            if data.is_alive() {
                data.send(mime_type, fd.as_fd());
            }
        }
    }
}

impl CompState {
    /// Offer the current primary selection to the focused
    /// client's devices — mirrors push_selection for the
    /// regular clipboard. An empty selection sends
    /// `selection(None)` so stale offers die on focus moves.
    pub fn push_primary(&mut self, surf: &wl_surface::WlSurface) {
        // Same display-borrow caveat as push_selection — clone
        // the handle first, never reach for self.display here.
        let handle = self.dh.clone();
        let Some(client) = surf.client() else { return };
        self.prim_devices.retain(|d| d.is_alive());
        for dev in self.prim_devices.clone() {
            if dev.client().map(|c| c.id()) != Some(client.id()) {
                continue;
            }
            match &self.primary {
                Some(source) if source.is_alive() => {
                    let mimes = source.mimes();
                    let offer = client
                        .create_resource::<ZwpPrimarySelectionOfferV1, SelSrc, CompState>(
                            &handle,
                            dev.version(),
                            source.clone(),
                        );
                    let Ok(offer) = offer else { continue };
                    dev.data_offer(&offer);
                    for m in mimes {
                        offer.offer(m);
                    }
                    dev.selection(Some(&offer));
                }
                _ => {
                    dev.selection(None);
                }
            }
        }
    }
}
