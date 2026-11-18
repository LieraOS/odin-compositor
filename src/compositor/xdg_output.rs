// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zxdg_output_manager_v1 — desktop-style output descriptions.
//!
//! wl_output carries the hardware view (mode, subpixel, physical
//! mm size); xdg_output adds the logical desktop view —
//! position inside the compositor layout, scaled size, a stable
//! connector name. Screenshot tools (grim), panel placement and
//! monitor-aware clients all resolve their target through this.

use wayland_protocols::xdg::xdg_output::zv1::server::{
    zxdg_output_manager_v1 as xom, zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1 as xo, zxdg_output_v1::ZxdgOutputV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

// Re-export — mod.rs names the global through this path.
pub use wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1 as ZxdgOutputManager;

impl Dispatch<ZxdgOutputManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgOutputManagerV1,
        request: xom::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xom::Request::GetXdgOutput { id, output } => {
                let xo = init.init::<ZxdgOutputV1, ()>(id, ());
                // The object must describe the wl_output it was
                // queried for — its bound resource carries the
                // connector name as udata, resolving to that
                // output's real geometry instead of a primary
                // guess. Logical size = mode at scale 1;
                // fractional scaling divides here once it lands.
                let name = output.data::<String>().cloned();
                let (x, y, w, h) = name
                    .as_deref()
                    .and_then(|n| state.layout.outputs.get(n))
                    .map(|o| (o.x, o.y, o.mode.0, o.mode.1))
                    .unwrap_or_else(|| state.layout.primary_rect());
                xo.logical_position(x, y);
                xo.logical_size(w as i32, h as i32);
                if xo.version() >= 2 {
                    let n = name.clone().unwrap_or_default();
                    xo.name(n.clone());
                    xo.description(format!("Odin output {n}"));
                }
                // zxdg_output.done is deprecated at v3 — the spec
                // substitutes a wl_output.done after the xdg
                // burst. It is LOAD-BEARING for Xwayland: its
                // apply_output_change only re-reads sizes on
                // wl_output.done, and wl_output mode/geometry are
                // deliberately skipped once an xdg_output object
                // exists. Without this second done the X screen
                // stays 0x0 — the sprite clamps to the origin and
                // every X11 pointer event dies there.
                if xo.version() < 3 {
                    xo.done();
                } else if output.version() >= 2 {
                    output.done();
                }
            }
            xom::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZxdgOutputV1,
        request: xo::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let _ = request;
    }
}
