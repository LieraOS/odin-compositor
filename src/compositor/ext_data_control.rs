// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_data_control_manager_v1 — the stabilized successor of
//! zwlr_data_control_manager_v1 for privileged clipboard access
//! (clipboard managers, newer GTK4/KDE tooling). The wire shape
//! mirrors the wlr original: manager → device/source/offer.
//!
//! Both protocols share one clipboard: sources land in
//! `state.selection`/`state.primary` as `SelSrc::CtrlExt` next
//! to the zwlr `SelSrc::Ctrl`, so offers, pastes and the X11
//! bridge stay protocol-agnostic. Manager devices get their own
//! list (`ext_ctrl_devices`) because the resource types differ.
//!
//! Only delta on the wire: ext devices get a `finished` event
//! when the seat they bind goes away — emitted on seat loss.
//!
//! Reference: wayland-protocols staging ext-data-control-v1.

use std::os::fd::AsFd;
use wayland_protocols::ext::data_control::v1::server::{
    ext_data_control_device_v1 as dcd, ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1 as dcm, ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1 as dco, ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1 as dcs, ext_data_control_source_v1::ExtDataControlSourceV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::data::SelSrc;
use super::data_control::CtrlSrcData;
use super::CompState;

pub use wayland_protocols::ext::data_control::v1::server::ext_data_control_manager_v1::ExtDataControlManagerV1 as ExtDataControlManager;

impl Dispatch<ExtDataControlManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtDataControlManagerV1,
        request: dcm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            dcm::Request::CreateDataSource { id } => {
                init.init::<ExtDataControlSourceV1, CtrlSrcData>(id, CtrlSrcData::default());
            }
            dcm::Request::GetDataDevice { id, .. } => {
                let dev = init.init::<ExtDataControlDeviceV1, ()>(id, ());
                state.ext_ctrl_devices.push(dev.clone());
                // A device that binds late must still see the
                // selection as it currently stands — replay it.
                state.ctrl_offer_to_ext(&dev, false);
                state.ctrl_offer_to_ext(&dev, true);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, CtrlSrcData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        src: &ExtDataControlSourceV1,
        request: dcs::Request,
        data: &CtrlSrcData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            dcs::Request::Offer { mime_type } => {
                data.mimes.lock().unwrap().push(mime_type);
            }
            dcs::Request::Destroy => {
                // A destroyed owner drops the clipboard with it —
                // everyone learns about the empty selection.
                if matches!(&state.selection, Some(SelSrc::CtrlExt(s)) if s == src) {
                    state.set_selection(None);
                }
                if matches!(&state.primary, Some(SelSrc::CtrlExt(s)) if s == src) {
                    state.set_primary(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtDataControlDeviceV1,
        request: dcd::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            dcd::Request::SetSelection { source } => {
                state.set_selection(source.map(SelSrc::CtrlExt));
            }
            dcd::Request::SetPrimarySelection { source } => {
                state.set_primary(source.map(SelSrc::CtrlExt));
            }
            dcd::Request::Destroy => {
                state.ext_ctrl_devices.retain(|d| d.is_alive());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, SelSrc> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtDataControlOfferV1,
        request: dco::Request,
        data: &SelSrc,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let dco::Request::Receive { mime_type, fd } = request {
            if let SelSrc::X(x) = data {
                // X-owned clipboard — the XWM pumps the
                // SelectionRequest, its reply fills this fd.
                state.x_paste_q.push((x.owner, mime_type, fd));
            } else {
                // Pipe the paste straight into the owning client's
                // send event — same model as wl_data_offer.receive.
                data.send(mime_type, fd.as_fd());
            }
        }
    }
}

impl CompState {
    /// Emit data_offer + selection (or primary_selection) to
    /// every bound ext manager device — the ext twin of
    /// announce_ctrl; both channels always hear the same owner.
    pub(crate) fn announce_ctrl_ext(&mut self, primary: bool) {
        self.ext_ctrl_devices.retain(|d| d.is_alive());
        for dev in self.ext_ctrl_devices.clone() {
            self.ctrl_offer_to_ext(&dev, primary);
        }
    }

    /// Build the ext offer object for one device and send the
    /// matching selection event — used by announce and replay.
    fn ctrl_offer_to_ext(&mut self, dev: &ExtDataControlDeviceV1, primary: bool) {
        let handle = self.dh.clone();
        let Some(client) = dev.client() else {
            return;
        };
        let src: Option<SelSrc> = if primary {
            self.primary.clone()
        } else {
            self.selection.clone()
        };
        match src.filter(|s| s.is_alive()) {
            Some(src) => {
                let Ok(offer) = client.create_resource::<ExtDataControlOfferV1, SelSrc, CompState>(
                    &handle,
                    dev.version(),
                    src.clone(),
                ) else {
                    return;
                };
                dev.data_offer(&offer);
                for m in src.mimes() {
                    offer.offer(m);
                }
                if primary {
                    dev.primary_selection(Some(&offer));
                } else {
                    dev.selection(Some(&offer));
                }
            }
            None => {
                if primary {
                    dev.primary_selection(None);
                } else {
                    dev.selection(None);
                }
            }
        }
    }
}
