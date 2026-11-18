// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_data_device — clipboard (selection) plumbing.
//!
//! Model: the client that owns the selection advertises mime
//! types through its wl_data_source. When keyboard focus lands
//! on a surface we build a wl_data_offer for that client's
//! data devices and send `selection(offer)`. On paste the
//! receiving client calls offer.receive(mime, fd) and we hand
//! that fd straight to source.send — the copy client writes
//! directly into the paste client's pipe, no pumping needed.
//!
//! Drag-and-drop requests are accepted but not yet routed.

use std::os::fd::{AsFd, BorrowedFd};
use tracing::info;
use wayland_protocols::ext::data_control::v1::server::ext_data_control_source_v1::ExtDataControlSourceV1;
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;
use wayland_server::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_surface,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::data_control::CtrlSrcData;
use super::CompState;

/// Per-source mime list — `offer` requests accumulate here so a
/// later wl_data_offer can mirror them to the paste client.
#[derive(Default)]
pub struct SourceData {
    pub mimes: std::sync::Mutex<Vec<String>>,
}

/// A clipboard owner regardless of the protocol that created
/// it — wl_data_source for normal clients, the data-control
/// source for clipboard managers. Offers, pastes and the X11
/// bridge all go through this so they never care which side
/// owns the selection right now.
#[derive(Clone)]
pub enum SelSrc {
    Wl(wl_data_source::WlDataSource),
    Ctrl(ZwlrDataControlSourceV1),
    /// ext_data_control_source_v1 — the stable-protocol
    /// successor channel; same clipboard, different front door.
    CtrlExt(ExtDataControlSourceV1),
    /// A wp_primary_selection owner — same source shape, used
    /// by the middle-click paste channel.
    Prim(ZwpPrimarySelectionSourceV1),
    /// An X client owns the selection — offers carry its mime
    /// list; pastes become SelectionRequests through the XWM.
    X(std::sync::Arc<XSelData>),
}

/// Compositor-side stand-in for an X-owned clipboard — no
/// wayland object backs it, so liveness is a flag we flip when
/// the X owner releases the selection.
pub struct XSelData {
    pub owner: u32,
    pub mimes: Vec<String>,
    pub dead: std::sync::atomic::AtomicBool,
}

impl SelSrc {
    pub fn is_alive(&self) -> bool {
        match self {
            Self::Wl(s) => s.is_alive(),
            Self::Ctrl(s) => s.is_alive(),
            Self::CtrlExt(s) => s.is_alive(),
            Self::Prim(s) => s.is_alive(),
            Self::X(x) => !x.dead.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// The mime types the owner advertised via offer requests.
    pub fn mimes(&self) -> Vec<String> {
        match self {
            Self::Wl(s) => s
                .data::<SourceData>()
                .map(|d| d.mimes.lock().unwrap().clone())
                .unwrap_or_default(),
            Self::Prim(s) => s
                .data::<SourceData>()
                .map(|d| d.mimes.lock().unwrap().clone())
                .unwrap_or_default(),
            Self::Ctrl(s) => s
                .data::<CtrlSrcData>()
                .map(|d| d.mimes.lock().unwrap().clone())
                .unwrap_or_default(),
            Self::CtrlExt(s) => s
                .data::<CtrlSrcData>()
                .map(|d| d.mimes.lock().unwrap().clone())
                .unwrap_or_default(),
            Self::X(x) => x.mimes.clone(),
        }
    }

    /// Ask the owner to write mime `mime` into `fd` — the
    /// request a paste translates into on both protocols.
    pub fn send(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Wl(s) => s.send(mime, fd),
            Self::Ctrl(s) => s.send(mime, fd),
            Self::CtrlExt(s) => s.send(mime, fd),
            Self::Prim(s) => s.send(mime, fd),
            // X sources have no client to send — the receive
            // dispatch routes them into x_paste_q instead.
            Self::X(_) => {}
        }
    }

    /// Selection moved to a new owner — the old one stops
    /// serving pastes from this moment on.
    pub fn cancel(&self) {
        match self {
            Self::Wl(s) => s.cancelled(),
            Self::Ctrl(s) => s.cancelled(),
            Self::CtrlExt(s) => s.cancelled(),
            Self::Prim(s) => s.cancelled(),
            Self::X(x) => x.dead.store(true, std::sync::atomic::Ordering::SeqCst),
        }
    }
}

/// Offer → owning source — receive() needs the source object to
/// trigger the source.send event on the offering client.
pub struct OfferData {
    pub source: SelSrc,
    /// Drag offers negotiate accept/actions/finish — selection
    /// offers only know receive + destroy.
    pub dnd: bool,
}

/// Bound data devices — selection pushes fan out to these.
/// Stored per state so focus changes can re-offer.

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                init.init::<wl_data_source::WlDataSource, SourceData>(id, SourceData::default());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let dev = init.init::<wl_data_device::WlDataDevice, ()>(id, ());
                state.data_devices.push(dev);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_source::WlDataSource,
        request: wl_data_source::Request,
        data: &SourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_source::Request::Offer { mime_type } => {
                data.mimes.lock().unwrap().push(mime_type);
            }
            wl_data_source::Request::Destroy => {
                // The selection's source went away — clear it so
                // paste clients get selection(None).
                if state
                    .selection
                    .as_ref()
                    .map(|s| !s.is_alive())
                    .unwrap_or(false)
                {
                    state.set_selection(None);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_device::WlDataDevice,
        request: wl_data_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device::Request::SetSelection { source, .. } => {
                info!("selection set");
                // The unified setter cancels the previous owner
                // and notifies data-control devices + X11 too.
                state.set_selection(source.map(SelSrc::Wl));
            }
            wl_data_device::Request::Release => {
                state.data_devices.retain(|d| d.is_alive());
            }
            wl_data_device::Request::StartDrag {
                source,
                origin: _,
                icon,
                ..
            } => {
                state.drag_start(source, icon);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        data: &OfferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_offer::Request::Receive { mime_type, fd } => {
                // The paste/drop client wants data — its fd goes
                // straight to the source's send event.
                if data.dnd {
                    state.drag_receive(mime_type, fd.as_fd());
                } else if let SelSrc::X(x) = &data.source {
                    // X-owned clipboard — queue the SelectionRequest;
                    // the XWM pumps it and the reply fills this fd.
                    state.x_paste_q.push((x.owner, mime_type, fd));
                } else if data.source.is_alive() {
                    data.source.send(mime_type, fd.as_fd());
                }
            }
            wl_data_offer::Request::Accept { mime_type, .. } => {
                if data.dnd {
                    state.drag_accept(mime_type);
                }
            }
            wl_data_offer::Request::SetActions { dnd_actions, .. } => {
                if data.dnd {
                    state.drag_actions(u32::from(dnd_actions));
                }
            }
            wl_data_offer::Request::Finish => {
                if data.dnd {
                    state.drag_finish();
                }
            }
            _ => {}
        }
    }
}

impl CompState {
    /// Offer the current selection to a client's data devices —
    /// called when keyboard focus moves to `surf`. With no
    /// selection set we send selection(None) so stale offers die.
    pub fn push_selection(&mut self, surf: &wl_surface::WlSurface) {
        // The cached handle stays valid while pump() has the
        // display taken out for dispatch_clients — reaching for
        // self.display here would panic inside request handlers.
        let handle = self.dh.clone();
        let Some(client) = surf.client() else { return };
        self.data_devices.retain(|d| d.is_alive());
        for dev in self.data_devices.clone() {
            if dev.client().map(|c| c.id()) != Some(client.id()) {
                continue;
            }
            match &self.selection {
                Some(source) if source.is_alive() => {
                    let mimes = source.mimes();
                    let offer = client
                        .create_resource::<wl_data_offer::WlDataOffer, OfferData, CompState>(
                            &handle,
                            dev.version(),
                            OfferData {
                                source: source.clone(),
                                dnd: false,
                            },
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
