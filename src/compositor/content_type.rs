// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `wp_content_type_v1` — content hints per surface
//! (none/photo/video/game). Double-buffered like all surface
//! state: `set_content_type` lands in `ctype_pending`, applies
//! at commit. The committed value is what renderer/output
//! policy reads (direct scanout, tearing, refresh decisions).

use wayland_protocols::wp::content_type::v1::server::{
    wp_content_type_manager_v1 as cm, wp_content_type_manager_v1::WpContentTypeManagerV1,
    wp_content_type_v1 as ct, wp_content_type_v1::WpContentTypeV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::wp::content_type::v1::server::wp_content_type_manager_v1::WpContentTypeManagerV1 as WpContentTypeManager;

impl Dispatch<WpContentTypeManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        res: &WpContentTypeManagerV1,
        request: cm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            cm::Request::GetSurfaceContentType { id, surface } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                init.init::<WpContentTypeV1, wl_surface::WlSurface>(id, surface.clone());
                let Some(sd) = surface.data::<SurfaceData>() else {
                    return;
                };
                // One content-type object per surface — duplicate
                // creation raises the manager's already_exists.
                let mut held = sd.ctype_obj.lock().unwrap();
                if *held {
                    res.post_error(
                        cm::Error::AlreadyConstructed,
                        "surface already has a content type object",
                    );
                    return;
                }
                *held = true;
            }
            cm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpContentTypeV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpContentTypeV1,
        request: ct::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ct::Request::SetContentType { content_type } => {
                if let Some(sd) = surface.data::<SurfaceData>() {
                    if let wayland_server::WEnum::Value(t) = content_type {
                        *sd.ctype_pending.lock().unwrap() = Some(t as u32);
                    }
                }
            }
            ct::Request::Destroy => {
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.ctype_obj.lock().unwrap() = false;
                }
            }
            _ => {}
        }
    }
}
