// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Hardware key dispatch — modifier tracking, compositor
//! bindings, repeat arming and client forwarding.

use std::time::Instant;
use tracing::info;

use super::CompState;

impl CompState {
    /// Key event on real input — track modifiers, fire bindings,
    /// manage repeat, then forward to the focused client.
    pub fn key_event(&mut self, key: u32, pressed: bool) {
        self.last_input = Instant::now();
        self.dirty = true;
        // evdev modifier keycodes → our C/A/S/W mask.
        let mod_bit = match key {
            29 | 97 => Some(1),   // LCTRL/RCTRL
            56 | 100 => Some(2),  // LALT/RALT
            42 | 54 => Some(4),   // LSHIFT/RSHIFT
            125 | 126 => Some(8), // LMETA/RMETA
            _ => None,
        };
        if let Some(bit) = mod_bit {
            let before = self.udev_mods;
            if pressed {
                self.udev_mods |= bit;
            } else {
                self.udev_mods &= !bit;
            }
            if self.udev_mods != before {
                self.push_modifiers();
            }
        }
        if pressed {
            // Session locked: compositor bindings are suspended —
            // except quit, the deliberate escape hatch if the
            // screenlock itself wedges.
            let binding_allowed = !self.kbd_inhibited()
                && (self.lock.is_none()
                    || self
                        .cfg
                        .bindings
                        .get(&(self.udev_mods, key))
                        .map(|a| a == "quit")
                        .unwrap_or(false));
            if binding_allowed {
                if let Some(action) = self.cfg.bindings.get(&(self.udev_mods, key)).cloned() {
                    if action == "toggle-layout" {
                        self.toggle_layout();
                    } else if action == "cycle-focus" {
                        self.cycle_focus();
                    } else if let Some(n) = action
                        .strip_prefix("ws-")
                        .and_then(|s| s.parse::<u8>().ok())
                    {
                        self.switch_ws(n - 1); // user-facing 1..=9 → internal 0..=8
                    } else if let Some(n) = action
                        .strip_prefix("mv-")
                        .and_then(|s| s.parse::<u8>().ok())
                    {
                        self.move_to_ws(n - 1);
                    } else if action == "dump-frame" {
                        self.dump_seq += 1;
                        self.dirty = true;
                        self.damage_everything();
                    } else if action == "quit" {
                        info!("binding: quit");
                        self.quit = true;
                    } else {
                        crate::spawn::run_binding(&action, self.x_display);
                    }
                    return; // compositor bindings are swallowed, never forwarded
                }
            }
            // Arm key repeat — modifiers and already-repeating keys
            // don't re-arm (a held key's repeats shouldn't restart
            // the 400ms delay each synthetic press).
            if mod_bit.is_none() {
                self.repeat_key = Some(key);
                self.repeat_next = Some(Instant::now() + std::time::Duration::from_millis(400));
            }
        } else if self.repeat_key == Some(key) {
            self.repeat_key = None;
            self.repeat_next = None;
        }
        self.send_key(key, pressed);
    }
}
