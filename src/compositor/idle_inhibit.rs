// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_idle_inhibit_manager_v1 — per-surface idle inhibition.
//!
//! Video players, presentation apps and games ask us not to
//! blank or lock while their surface is visible. The tracked
//! inhibitor set is the ground truth a later idle/DPMS or
//! lockscreen path queries via `idle_inhibited()` — surfaces
//! keep their veto only while both the surface and the
//! inhibitor object are alive.

use tracing::info;
use wayland_protocols::wp::idle_inhibit::zv1::server::{
    zwp_idle_inhibit_manager_v1 as iim, zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1 as ii, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols::wp::idle_inhibit::zv1::server::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1 as ZwpIdleInhibitManager;

/// One live (surface, inhibitor) pair — the tuple CompState stores.
pub type Inhibitor = (wl_surface::WlSurface, ZwpIdleInhibitorV1);

impl CompState {
    /// Whether `surf` currently vetoes idle/blank — the query
    /// the DPMS/lock path calls once that timer exists.
    #[allow(dead_code)]
    pub fn idle_inhibited(&self, surf: &wl_surface::WlSurface) -> bool {
        self.idle_inhibitors
            .iter()
            .any(|(s, i)| s.id() == surf.id() && i.is_alive() && s.is_alive())
    }
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpIdleInhibitManagerV1,
        request: iim::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let iim::Request::CreateInhibitor { id, surface } = request {
            let inh = init.init::<ZwpIdleInhibitorV1, ()>(id, ());
            state.idle_inhibitors.push((surface, inh));
            info!("idle inhibitor registered");
        }
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        inh: &ZwpIdleInhibitorV1,
        request: ii::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ii::Request::Destroy = request {
            state.idle_inhibitors.retain(|(_, i)| i != inh);
            info!("idle inhibitor released");
        }
    }
}
