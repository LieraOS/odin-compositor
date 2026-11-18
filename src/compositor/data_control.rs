// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_data_control_manager_v1 — privileged clipboard access
//! for clipboard managers (wl-clipboard: wl-copy/wl-paste).
//!
//! The same clipboard lives behind two front doors: normal
//! clients own the selection through wl_data_device; manager
//! clients through this protocol. Both land in `state.selection`
//! as a `SelSrc` so offers, pastes and the X11 bridge never care
//! which side set it. Manager devices additionally observe the
//! primary selection, which has no wl_data_device counterpart.

use std::os::fd::AsFd;
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1 as dcd, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1 as dcm, zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1 as dco, zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1 as dcs, zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::data::SelSrc;
use super::CompState;

pub use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as ZwlrDataControlManager;

/// Mime list of a manager-owned source — same role SourceData
/// plays for wl_data_source.
#[derive(Default)]
pub struct CtrlSrcData {
    pub mimes: std::sync::Mutex<Vec<String>>,
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrDataControlManagerV1,
        request: dcm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            dcm::Request::CreateDataSource { id } => {
                init.init::<ZwlrDataControlSourceV1, CtrlSrcData>(id, CtrlSrcData::default());
            }
            dcm::Request::GetDataDevice { id, .. } => {
                let dev = init.init::<ZwlrDataControlDeviceV1, ()>(id, ());
                state.ctrl_devices.push(dev.clone());
                // A device that binds late must still see the
                // selection as it currently stands — replay it.
                state.ctrl_offer_to(&dev, false);
                state.ctrl_offer_to(&dev, true);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, CtrlSrcData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        src: &ZwlrDataControlSourceV1,
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
                if matches!(&state.selection, Some(SelSrc::Ctrl(s)) if s == src) {
                    state.set_selection(None);
                }
                if matches!(&state.primary, Some(SelSrc::Ctrl(s)) if s == src) {
                    state.set_primary(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrDataControlDeviceV1,
        request: dcd::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            dcd::Request::SetSelection { source } => {
                state.set_selection(source.map(SelSrc::Ctrl));
            }
            dcd::Request::SetPrimarySelection { source } => {
                state.set_primary(source.map(SelSrc::Ctrl));
            }
            dcd::Request::Destroy => {
                state.ctrl_devices.retain(|d| d.is_alive());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, SelSrc> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrDataControlOfferV1,
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
    /// Replace the clipboard owner. The old source is cancelled
    /// so it stops serving pastes; both device families and the
    /// X11 bridge are told about the new owner.
    pub fn set_selection(&mut self, src: Option<SelSrc>) {
        if let Some(old) = self.selection.take() {
            old.cancel();
        }
        self.selection = src.filter(|s| s.is_alive());
        // An X-owned source must NOT be claimed or cleared by us —
        // wm_win owning CLIPBOARD means the *Wayland* side owns it.
        if !matches!(self.selection, Some(crate::compositor::data::SelSrc::X(_))) {
            self.xreqs.push(if self.selection.is_some() {
                crate::xwm::XReq::OwnClipboard
            } else {
                crate::xwm::XReq::ClearClipboard
            });
        }
        // Normal clients learn through their data devices when
        // they (re-)gain focus — the focused one right now.
        if let Some(surf) = self.kbd_focus.clone() {
            self.push_selection(&surf);
        }
        self.announce_ctrl(false);
        self.announce_ctrl_ext(false);
    }

    /// Replace the primary selection — one SelSrc slot shared
    /// by data-control managers and wp_primary_selection
    /// owners. Cancels the previous owner and republishes to
    /// manager devices plus the focused client's prim devices.
    pub fn set_primary(&mut self, src: Option<SelSrc>) {
        if let Some(old) = self.primary.take() {
            old.cancel();
        }
        self.primary = src.filter(|s| s.is_alive());
        self.announce_ctrl(true);
        self.announce_ctrl_ext(true);
        // Focused client's wp devices track primary the same
        // way the regular clipboard does.
        if let Some(surf) = self.kbd_focus.clone() {
            self.push_primary(&surf);
        }
    }

    /// Emit data_offer + selection (or primary_selection) to
    /// every bound manager device — the observation channel
    /// wl-paste-style watchers live on.
    fn announce_ctrl(&mut self, primary: bool) {
        self.ctrl_devices.retain(|d| d.is_alive());
        for dev in self.ctrl_devices.clone() {
            self.ctrl_offer_to(&dev, primary);
        }
    }

    /// Build the offer object for one device and send the
    /// matching selection event — used by announce and replay.
    fn ctrl_offer_to(&mut self, dev: &ZwlrDataControlDeviceV1, primary: bool) {
        let handle = self.dh.clone();
        let Some(client) = dev.client() else { return };
        let src: Option<SelSrc> = if primary {
            self.primary.clone()
        } else {
            self.selection.clone()
        };
        match src.filter(|s| s.is_alive()) {
            Some(src) => {
                let Ok(offer) = client
                    .create_resource::<ZwlrDataControlOfferV1, SelSrc, CompState>(
                        &handle,
                        dev.version(),
                        src.clone(),
                    )
                else {
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
