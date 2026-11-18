// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-input dispatch for the nested backend — pointer and
//! keyboard events arriving from the host compositor forward
//! into our own seat through the same paths real hardware uses.

use wayland_client::protocol::{wl_keyboard, wl_pointer};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use super::nested::Nested;

impl Dispatch<wl_pointer::WlPointer, ()> for Nested {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.ptr_events += 1;
        // Surface-local coords are global coords here — the nested
        // window IS the whole compositor space.
        let comp = state.comp.as_mut();
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(c) = comp {
                    c.ptr_motion(surface_x, surface_y);
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(c) = comp {
                    c.ptr_motion(surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button {
                button, state: st, ..
            } => {
                if let Some(c) = comp {
                    c.ptr_button(
                        button,
                        matches!(st, WEnum::Value(wl_pointer::ButtonState::Pressed)),
                    );
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Nested {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.key_events += 1;
        // Raw key events through the real path — it tracks
        // modifiers, fires bindings, and forwards to the focused
        // client. Same call the libinput backend makes.
        if let wl_keyboard::Event::Key { key, state: st, .. } = event {
            if let Some(c) = state.comp.as_mut() {
                c.key_event(
                    key,
                    matches!(st, WEnum::Value(wl_keyboard::KeyState::Pressed)),
                );
            }
        }
    }
}
