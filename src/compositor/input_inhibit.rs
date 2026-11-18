// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_input_inhibit_manager_v1 — the screen-locker contract.
//!
//! One client (e.g. swaylock) holds the inhibitor at a time;
//! while it lives, ONLY that client's surfaces receive pointer
//! and keyboard events and appear in hit tests. Everything else
//! is invisible to input until the inhibitor object dies.

use tracing::info;
use wayland_protocols_wlr::input_inhibitor::v1::server::{
    zwlr_input_inhibit_manager_v1 as iim, zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1,
    zwlr_input_inhibitor_v1 as ii, zwlr_input_inhibitor_v1::ZwlrInputInhibitorV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols_wlr::input_inhibitor::v1::server::zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1 as ZwlrInputInhibitManager;

/// udata marker — we only need to know which client owns it.

impl CompState {
    /// Whether `client` may receive input right now — true for
    /// everyone while no inhibitor is held, and only for the
    /// inhibiting client while a lock screen owns the session.
    pub(crate) fn input_allowed(&self, client: Option<&Client>) -> bool {
        match &self.inhibitor {
            // A dead inhibitor object (client gone, prune not run
            // yet) must not darken input — treat it as released.
            Some(inhib) if inhib.is_alive() => client
                .and_then(|c| inhib.client().map(|i| c.id() == i.id()))
                .unwrap_or(false),
            _ => true,
        }
    }
}

impl Dispatch<ZwlrInputInhibitManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        client: &Client,
        mgr: &ZwlrInputInhibitManagerV1,
        request: iim::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let iim::Request::GetInhibitor { id } = request {
            // A dead inhibitor (client quit or destroyed it) no
            // longer counts — only a live one blocks creation.
            if state
                .inhibitor
                .as_ref()
                .map(|i| i.is_alive())
                .unwrap_or(false)
            {
                // The id must be born before the error return —
                // an uninit'd object panics the wayland backend
                // and kills the compositor, not just the client.
                init.init::<ZwlrInputInhibitorV1, ()>(id, ());
                // One inhibitor at a time, per the error enum —
                // a second locker racing in gets a protocol
                // error instead of silently sharing the screen.
                mgr.post_error(
                    iim::Error::AlreadyInhibited,
                    "an input inhibitor is already active",
                );
                return;
            }
            let _ = client;
            let obj = init.init::<ZwlrInputInhibitorV1, ()>(id, ());
            state.inhibitor = Some(obj);
            info!("input inhibitor active — other clients go dark");
        }
    }
}

impl Dispatch<ZwlrInputInhibitorV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrInputInhibitorV1,
        request: ii::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ii::Request::Destroy = request {
            state.inhibitor = None;
            info!("input inhibitor released — input restored");
        }
    }
}
