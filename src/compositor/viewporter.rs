// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wp_viewporter — per-surface crop and scale.
//!
//! Games and SDL-style clients render a small buffer and let the
//! compositor scale it to the window size; fractional scaling
//! machinery uses the same primitive. `set_source` picks a
//! (fractional) buffer-space crop, `set_destination` the
//! surface-local size it displays as — both are pending state
//! applied on wl_surface.commit like every surface attribute.

use wayland_protocols::wp::viewporter::server::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::wp::viewporter::server::wp_viewporter::WpViewporter as WpViewporterGlobal;

/// Crop+scale state for one surface — `src` is the buffer-space
/// crop (fractional per spec, unset = whole buffer), `dst` the
/// surface-local size the crop displays as (unset = src extent).
#[derive(Clone, Copy, Default)]
pub struct Viewport {
    pub src: Option<(f64, f64, f64, f64)>,
    pub dst: Option<(i32, i32)>,
}

impl CompState {
    /// The size a surface presents as to input and layout:
    /// viewport destination wins, the src crop extent is the
    /// fallback, and the raw buffer dims apply when no viewport
    /// object exists at all.
    pub(crate) fn surf_size(&self, sd: &SurfaceData) -> (i32, i32) {
        let vp = *sd.vp.lock().unwrap();
        if let Some((w, h)) = vp.dst {
            return (w, h);
        }
        if let Some((_, _, w, h)) = vp.src {
            return (w.ceil() as i32, h.ceil() as i32);
        }
        sd.shown_buf
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.data::<super::BufInfo>())
            .map(|bi| (bi.w, bi.h))
            .unwrap_or((0, 0))
    }
}

impl Dispatch<WpViewporter, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpViewporter,
        request: wp_viewporter::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_viewporter::Request::GetViewport { id, surface } => {
                // The surface becomes the viewport's dispatch
                // data — requests write straight into its
                // pending slot.
                init.init::<WpViewport, wl_surface::WlSurface>(id, surface);
            }
            wp_viewporter::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpViewport, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        obj: &WpViewport,
        request: wp_viewport::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        match request {
            wp_viewport::Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                if x == -1.0 && y == -1.0 && width == -1.0 && height == -1.0 {
                    sd.vp_pending.lock().unwrap().src = None;
                } else if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                    // Spec: out-of-range crops are protocol errors.
                    obj.post_error(wp_viewport::Error::BadValue, "source rect out of range");
                } else {
                    sd.vp_pending.lock().unwrap().src = Some((x, y, width, height));
                }
            }
            wp_viewport::Request::SetDestination { width, height } => {
                if width == -1 && height == -1 {
                    sd.vp_pending.lock().unwrap().dst = None;
                } else if width <= 0 || height <= 0 {
                    obj.post_error(
                        wp_viewport::Error::BadValue,
                        "destination size out of range",
                    );
                } else {
                    sd.vp_pending.lock().unwrap().dst = Some((width, height));
                }
            }
            wp_viewport::Request::Destroy => {}
            _ => {}
        }
    }
}
