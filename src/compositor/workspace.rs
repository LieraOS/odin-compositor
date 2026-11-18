// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Workspaces — the i3 ownership model.
//!
//! A workspace belongs to the output showing it; a window
//! belongs to a workspace. A window is visible exactly when
//! some enabled output displays its workspace — tiling,
//! rendering, hit-testing and focus cycling all filter on
//! that one predicate. Switching steals a workspace shown
//! elsewhere: the losing output falls back to its previous
//! workspace rather than going blank.

use tracing::info;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::Resource;

use super::{CompState, SurfaceData};

/// Number of workspaces — 1..=9 user-facing (0..=8 internal).
pub const WS_COUNT: u8 = 9;

impl CompState {
    /// Whether `surf`'s window is on a workspace some enabled
    /// output currently shows — the single visibility gate
    /// every consumer (tile, render, hit, focus) shares.
    pub fn ws_visible(&self, surf: &WlSurface) -> bool {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return true; // aux surfaces are not workspace-bound
        };
        let ws = *sd.ws.lock().unwrap();
        self.layout
            .outputs
            .values()
            .any(|o| o.enabled && o.ws == ws)
    }

    /// Switch the focused output's visible workspace to `n`.
    /// If another output already shows `n` the workspace moves
    /// here — the loser falls back to its previous workspace.
    pub fn switch_ws(&mut self, n: u8) {
        if n >= WS_COUNT {
            return;
        }
        // Which output takes the switch: the focused window's,
        // else the one under the pointer, else primary.
        let target = self
            .kbd_focus
            .as_ref()
            .and_then(|s| self.output_of(s))
            .or_else(|| {
                self.layout
                    .outputs
                    .values()
                    .find(|o| {
                        self.ptr_pos.0 >= o.x as f64
                            && self.ptr_pos.0 < (o.x + o.mode.0 as i32) as f64
                    })
                    .cloned()
            })
            .or_else(|| self.layout.outputs.values().find(|o| o.primary).cloned())
            .or_else(|| self.layout.outputs.values().next().cloned());
        let Some(target) = target else { return };
        if target.ws == n {
            return; // already there
        }
        let name = target.name.clone();
        self.show_ws(&name, n);
    }

    /// Put workspace `n` on the named output — the shared core
    /// of `switch_ws` (keybinds pick the output) and
    /// ext_workspace's `assign` (the client names it). Same
    /// steal, layout and focus consequences either way.
    pub fn show_ws(&mut self, name: &str, n: u8) {
        // Steal handling: another output showing `n` falls back
        // to what it displayed before, never to a blank screen.
        for o in self.layout.outputs.values_mut() {
            if o.enabled && o.ws == n && o.name != name {
                o.ws = o.prev_ws;
                info!(output = %o.name, ws = o.ws, "workspace stolen — output reverted");
            }
        }
        let Some(o) = self.layout.outputs.get_mut(name) else {
            return;
        };
        if o.ws == n {
            return;
        }
        info!(output = %name, from = o.ws, to = n, "workspace switch");
        o.prev_ws = o.ws;
        o.ws = n;
        self.apply_layout();
        // Focus lands on the top visible window of the new ws —
        // or nowhere when it's empty.
        let top = self
            .order
            .iter()
            .rev()
            .find(|s| self.ws_visible(s))
            .cloned();
        self.set_kbd_focus(top);
        self.damage_everything();
    }

    /// Move the focused window to workspace `n` — it vanishes
    /// here and appears on whichever output shows `n` (or hides
    /// until that workspace is switched to).
    pub fn move_to_ws(&mut self, n: u8) {
        if n >= WS_COUNT {
            return;
        }
        let Some(surf) = self.kbd_focus.clone() else {
            return;
        };
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        *sd.ws.lock().unwrap() = n;
        info!(ws = n, "window moved to workspace");
        self.apply_layout();
        // Focus follows the compositor convention: it stays on
        // the current workspace's next window, not the moved one.
        if !self.ws_visible(&surf) {
            let top = self
                .order
                .iter()
                .rev()
                .find(|s| self.ws_visible(s))
                .cloned();
            self.set_kbd_focus(top);
        }
        self.damage_everything();
    }

    /// Assign a freshly mapped window its workspace — the one
    /// its output currently shows, so new windows appear on
    /// the desktop the user is looking at.
    pub fn ws_assign_new(&self, surf: &WlSurface) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        let ws = self.output_of(surf).map(|o| o.ws).unwrap_or_else(|| {
            self.layout
                .outputs
                .values()
                .find(|o| o.primary)
                .map(|o| o.ws)
                .unwrap_or(0)
        });
        *sd.ws.lock().unwrap() = ws;
    }
}
