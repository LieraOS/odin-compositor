// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `wp_tearing_control_v1` — per-surface presentation hint.
//! Games and latency-critical clients opt out of vsync via
//! `presentation_hint(async)`. The hint is double-buffered
//! surface state: pending until commit, then the committed flag
//! feeds the DRM page-flip flags (`comp.tearing_active`).

use wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1 as tcm, wp_tearing_control_manager_v1::WpTearingControlManagerV1,
    wp_tearing_control_v1 as tc, wp_tearing_control_v1::WpTearingControlV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::{CompState, SurfaceData};

pub use wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1 as WpTearingManager;

impl Dispatch<WpTearingControlManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        res: &WpTearingControlManagerV1,
        request: tcm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            tcm::Request::GetTearingControl { id, surface } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                init.init::<WpTearingControlV1, wl_surface::WlSurface>(id, surface.clone());
                let Some(sd) = surface.data::<SurfaceData>() else {
                    return;
                };
                // One tearing-control object per surface — the
                // protocol raises already_exists on duplicates.
                let mut held = sd.tear_obj.lock().unwrap();
                if *held {
                    res.post_error(
                        tcm::Error::TearingControlExists,
                        "surface already has tearing control",
                    );
                    return;
                }
                *held = true;
            }
            tcm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpTearingControlV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpTearingControlV1,
        request: tc::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            tc::Request::SetPresentationHint { hint } => {
                let async_hint = matches!(hint, WEnum::Value(tc::PresentationHint::Async));
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.tearing_pending.lock().unwrap() = Some(async_hint);
                }
            }
            tc::Request::Destroy => {
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.tear_obj.lock().unwrap() = false;
                }
            }
            _ => {}
        }
    }
}

impl CompState {
    /// Any committed surface asking for async presentation —
    /// the DRM page-flip path consults this for the ASYNC flag.
    pub(crate) fn tearing_active(&self) -> bool {
        self.order.iter().any(|s| {
            s.data::<SurfaceData>()
                .map(|sd| *sd.tearing.lock().unwrap())
                .unwrap_or(false)
        })
    }
}
