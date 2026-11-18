// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wp_fractional_scale_v1 — the compositor tells each surface
//! which scale its output prefers. Clients answer by rendering
//! denser buffers and pulling them back down through
//! wp_viewport's destination rect — the two protocols are
//! designed to work as a pair.
//!
//! The scale itself lives on `wm::Output` (output-management's
//! set_scale applies it); the event re-sends whenever a
//! surface's effective output scale changes — checked lazily on
//! commit, which covers "moved to a differently-scaled monitor"
//! without any extra hooks in the move paths.

use wayland_protocols::wp::fractional_scale::v1::server::{
    wp_fractional_scale_manager_v1 as fsm, wp_fractional_scale_v1 as fs1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

// Re-export — mod.rs names the global through this path.
pub use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1 as WpFractionalScaleManager;
pub use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_v1::WpFractionalScaleV1;

/// The surface's preferred scale ×120 (the protocol's fixed
/// denominator) — resolved through the window-center output.
/// Unmapped/aux surfaces fall back to the primary output.
pub fn preferred(state: &CompState, surf: &wl_surface::WlSurface) -> u32 {
    let scale = state.output_of(surf).map(|o| o.scale).unwrap_or_else(|| {
        state
            .layout
            .outputs
            .values()
            .find(|o| o.primary)
            .map(|o| o.scale)
            .unwrap_or(1.0)
    });
    // Never report below 1.0 — a nonsensical config value would
    // otherwise tell clients to render blurry.
    (scale.max(1.0) * 120.0).round() as u32
}

/// Re-announce when the effective scale moved — called from
/// wl_surface.commit so output-hops and config applies both land
/// without dedicated hooks.
pub fn refresh(state: &CompState, surf: &wl_surface::WlSurface, sd: &SurfaceData) {
    let want = preferred(state, surf);
    let mut last = sd.fs_scale.lock().unwrap();
    if *last == want {
        return;
    }
    *last = want;
    if let Some(obj) = sd.fs_obj.lock().unwrap().as_ref().filter(|o| o.is_alive()) {
        obj.preferred_scale(want);
    }
}

impl Dispatch<WpFractionalScaleManager, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpFractionalScaleManager,
        request: fsm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            fsm::Request::GetFractionalScale { id, surface } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                let obj =
                    init.init::<WpFractionalScaleV1, wl_surface::WlSurface>(id, surface.clone());
                if let Some(sd) = surface.data::<SurfaceData>() {
                    if sd.fs_obj.lock().unwrap().is_some() {
                        // One addon object per surface, per spec.
                        surface.post_error(
                            fsm::Error::FractionalScaleExists,
                            "surface already has wp_fractional_scale_v1",
                        );
                        return;
                    }
                    *sd.fs_obj.lock().unwrap() = Some(obj.clone());
                    // The initial event is mandatory — clients
                    // won't commit a scaled buffer before it.
                    let scale = preferred(state, &surface);
                    *sd.fs_scale.lock().unwrap() = scale;
                    obj.preferred_scale(scale);
                }
            }
            fsm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        obj: &WpFractionalScaleV1,
        request: fs1::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let fs1::Request::Destroy = request {
            // Detach so commit-time refresh stops talking to a
            // dead object — and so a fresh get_fractional_scale
            // can re-arm the surface.
            if let Some(sd) = surf.data::<SurfaceData>() {
                let mut slot = sd.fs_obj.lock().unwrap();
                if slot.as_ref().is_some_and(|o| o.id() == obj.id()) {
                    *slot = None;
                    *sd.fs_scale.lock().unwrap() = 0;
                }
            }
        }
    }
}
