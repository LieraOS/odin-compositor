// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `wp_alpha_modifier_v1` — per-surface alpha multiplier.
//! `set_multiplier` is double-buffered: it lands in
//! `alpha_pending` and applies at commit, like every other
//! wl_surface state. The committed multiplier flows into both
//! compositing paths (CPU blit factor, Vulkan push constant).

use wayland_protocols::wp::alpha_modifier::v1::server::{
    wp_alpha_modifier_surface_v1 as ams, wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
    wp_alpha_modifier_v1 as am, wp_alpha_modifier_v1::WpAlphaModifierV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::wp::alpha_modifier::v1::server::wp_alpha_modifier_v1::WpAlphaModifierV1 as WpAlphaManager;

impl Dispatch<WpAlphaModifierV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        res: &WpAlphaModifierV1,
        request: am::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            am::Request::GetSurface { id, surface } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                init.init::<WpAlphaModifierSurfaceV1, wl_surface::WlSurface>(id, surface.clone());
                let Some(sd) = surface.data::<SurfaceData>() else {
                    return;
                };
                // One alpha-modifier object per surface — the
                // protocol raises already_exists on duplicates.
                let mut held = sd.alpha_obj.lock().unwrap();
                if *held {
                    res.post_error(
                        am::Error::AlreadyConstructed,
                        "surface already has an alpha modifier",
                    );
                    return;
                }
                *held = true;
            }
            am::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpAlphaModifierSurfaceV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpAlphaModifierSurfaceV1,
        request: ams::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ams::Request::SetMultiplier { factor } => {
                if let Some(sd) = surface.data::<SurfaceData>() {
                    // The factor is a uint where u32::MAX is full
                    // opacity — normalize to 0.0..=1.0; 1.0 stores
                    // as None so the render paths keep their
                    // identity fast-path.
                    let a = factor as f64 / u32::MAX as f64;
                    *sd.alpha_pending.lock().unwrap() = Some(a);
                }
            }
            ams::Request::Destroy => {
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.alpha_obj.lock().unwrap() = false;
                }
            }
            _ => {}
        }
    }
}
