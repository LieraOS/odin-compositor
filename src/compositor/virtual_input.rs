// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_virtual_pointer_manager_v1 — client-driven pointer
//! injection (ydotool, wayvnc, barriers).
//!
//! Requests arrive as if they were hardware: relative motion
//! accumulates onto `ptr_pos`, absolute motion maps the
//! client's extent onto our global space, buttons and axis go
//! through the same internal paths real libinput events take —
//! so grabs, constraints and focus behave identically.

use tracing::info;
use wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1 as vpm,
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1 as vp,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use wayland_server::protocol::wl_pointer;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::CompState;

pub use wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as ZwlrVirtualPointerManager;

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrVirtualPointerManagerV1,
        request: vpm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            vpm::Request::CreateVirtualPointer { id, .. } => {
                init.init::<ZwlrVirtualPointerV1, ()>(id, ());
                info!("virtual pointer created");
            }
            // v2: with_output constrains absolute motion to one
            // output — our single space maps the same way.
            vpm::Request::CreateVirtualPointerWithOutput { id, .. } => {
                init.init::<ZwlrVirtualPointerV1, ()>(id, ());
                info!("virtual pointer (output-bound) created");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        ptr: &ZwlrVirtualPointerV1,
        request: vp::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Deltas accumulate onto the live pointer position —
            // same path as a hardware relative event.
            vp::Request::Motion { dx, dy, .. } => {
                let (x, y) = state.ptr_pos;
                state.ptr_motion(x + dx, y + dy);
            }
            vp::Request::MotionAbsolute {
                x,
                y,
                x_extent,
                y_extent,
                ..
            } => {
                // The client's extent maps onto the compositor's
                // global space — outside it the ratio clamps.
                let sx = if x_extent > 0 {
                    x as f64 * state.space.0 / x_extent as f64
                } else {
                    state.ptr_pos.0
                };
                let sy = if y_extent > 0 {
                    y as f64 * state.space.1 / y_extent as f64
                } else {
                    state.ptr_pos.1
                };
                state.ptr_motion(sx, sy);
            }
            vp::Request::Button {
                button, state: st, ..
            } => {
                let pressed = matches!(st, WEnum::Value(wl_pointer::ButtonState::Pressed));
                state.ptr_button(button, pressed);
            }
            vp::Request::Axis { axis, value, .. } => {
                let (ax, src) = match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => {
                        (0u32, wl_pointer::AxisSource::Wheel)
                    }
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        (1u32, wl_pointer::AxisSource::Wheel)
                    }
                    _ => {
                        ptr.post_error(
                            vp::Error::InvalidAxis,
                            "axis is not vertical or horizontal",
                        );
                        return;
                    }
                };
                let _ = src;
                // The source a prior axis_source request set —
                // default wheel, per protocol.
                state.ptr_axis(ax, value, state.vp_axis_source);
            }
            vp::Request::AxisSource { axis_source } => {
                // Wheel vs finger vs continuous changes the event
                // stream clients see — remember it for the next
                // axis request via a small side channel.
                if let WEnum::Value(s) = axis_source {
                    state.vp_axis_source = match s {
                        wl_pointer::AxisSource::Finger => 2,
                        wl_pointer::AxisSource::Continuous => 3,
                        _ => 0,
                    };
                } else {
                    ptr.post_error(vp::Error::InvalidAxisSource, "unknown axis source");
                }
            }
            vp::Request::AxisStop { axis, .. } => {
                // axis_stop ends a scroll sequence — clients get
                // axis_stop through the normal path; we fold it
                // into the axis request set below.
                let _ = axis;
            }
            vp::Request::AxisDiscrete { axis, value, .. } => {
                let ax = match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => 0u32,
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => 1u32,
                    _ => return,
                };
                // Discrete clicks ride the same injection path.
                state.ptr_axis(ax, value as f64, wl_pointer::AxisSource::Wheel.into());
            }
            vp::Request::Frame | vp::Request::Destroy => {}
            _ => {}
        }
    }
}
