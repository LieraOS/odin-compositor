// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Wayland-client dispatch impls for the nested backend — registry
//! binding, the sinks every unused interface needs, and the
//! xdg_surface/xdg_toplevel handlers that drive configure-acked
//! resizes and host-initiated close. Split from nested.rs to keep
//! both files inside the size budget.

use tracing::info;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::nested::Nested;

impl Dispatch<wl_registry::WlRegistry, ()> for Nested {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, 7, qh, ()));
                }
                "xdg_wm_base" => {
                    state.xdg = Some(registry.bind(name, 3, qh, ()));
                }
                _ => {}
            }
        }
    }
}

macro_rules! sink {
    ($iface:ty, $data:ty) => {
        impl Dispatch<$iface, $data> for Nested {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as wayland_client::Proxy>::Event,
                _: &$data,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
sink!(wl_compositor::WlCompositor, ());
sink!(wl_shm::WlShm, ());
sink!(wl_shm_pool::WlShmPool, ());
sink!(wl_buffer::WlBuffer, ());
sink!(wl_seat::WlSeat, ());

impl Dispatch<wl_surface::WlSurface, ()> for Nested {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Nested {
    fn event(
        state: &mut Self,
        surf: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surf.ack_configure(serial);
            state.configured = true;
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Nested {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => {
                info!("nested window closed by host — exiting");
                std::process::exit(0);
            }
            // The host dictates our size (tiling, maximize) —
            // record it so the frame loop can rebuild buffer and
            // virtual output at the assigned extent.
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 && (width as u32, height as u32) != state.win_size {
                    state.win_size = (width as u32, height as u32);
                    state.resize_pending = true;
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Nested {
    fn event(
        _: &mut Self,
        _: &xdg_wm_base::XdgWmBase,
        _: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
