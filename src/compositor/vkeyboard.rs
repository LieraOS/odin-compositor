// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_virtual_keyboard_manager_v1 — client-driven key
//! injection (ydotool, wayvnc, input methods).
//!
//! Key events enter through `key_event` — the same path real
//! libinput keys take — so modifier tracking, compositor
//! bindings and repeat behave identically. The client-supplied
//! keymap is validated and kept; injected keys arrive as evdev
//! keycodes, matching how hardware keys feed the seat.

use std::io::Read;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1 as vkm,
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1 as vk,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_server::protocol::wl_keyboard;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as ZwpVirtualKeyboardManager;

/// Per-keyboard keymap — validated at upload, kept so a later
/// keymap-aware path can translate keysyms if needed.
#[derive(Default)]
pub struct VkData {
    pub keymap: std::sync::Mutex<Option<Vec<u8>>>,
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpVirtualKeyboardManagerV1,
        request: vkm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let vkm::Request::CreateVirtualKeyboard { id, .. } = request {
            init.init::<ZwpVirtualKeyboardV1, VkData>(id, VkData::default());
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, VkData> for CompState {
    fn request(
        state: &mut Self,
        client: &Client,
        kb: &ZwpVirtualKeyboardV1,
        request: vk::Request,
        data: &VkData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            vk::Request::Keymap { format, fd, size } => {
                // Read the keymap off the client's fd — invalid
                // formats/fds are a protocol error, not a panic.
                let ok_format = format == u32::from(wl_keyboard::KeymapFormat::XkbV1)
                    || format == u32::from(wl_keyboard::KeymapFormat::NoKeymap);
                let mut buf = Vec::with_capacity(size as usize);
                let read = std::fs::File::from(fd)
                    .take(size as u64)
                    .read_to_end(&mut buf);
                if !ok_format || read.map(|n| n != size as usize).unwrap_or(true) {
                    // Invalid keymap input — the protocol carries no
                    // error enum, so a display-level error ends it.
                    kb.post_error(0u32, "virtual keyboard keymap unreadable");
                    return;
                }
                *data.keymap.lock().unwrap() = Some(buf);
            }
            vk::Request::Key { key, state: st, .. } => {
                // Inhibition applies to injected keys as much as
                // to hardware ones — a locker still owns input.
                if !state.input_allowed(Some(client)) {
                    return;
                }
                let pressed = st == u32::from(wl_keyboard::KeyState::Pressed);
                state.key_event(key, pressed);
            }
            vk::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                ..
            } => {
                // XKB mod indices → our C/A/S/W mask: the common
                // positions are Shift=0 Ctrl=2 Alt(Mod1)=3 Super(Mod4)=6.
                let eff = mods_depressed | mods_latched | mods_locked;
                let mut m = 0u32;
                if eff & (1 << 0) != 0 {
                    m |= 4;
                } // Shift
                if eff & (1 << 2) != 0 {
                    m |= 1;
                } // Ctrl
                if eff & (1 << 3) != 0 {
                    m |= 2;
                } // Alt
                if eff & (1 << 6) != 0 {
                    m |= 8;
                } // Super
                state.udev_mods = m;
                state.push_modifiers();
            }
            _ => {}
        }
    }
}
