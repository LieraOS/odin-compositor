// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_keyboard_shortcuts_inhibit_v1 — a focused surface can
//! claim ALL key events, suspending compositor bindings while
//! the inhibition is active. VMs and games need this to give
//! their guest/emulated OS the full keyboard.
//!
//! Policy: the inhibitor goes active when its surface holds
//! keyboard focus, inactive when focus moves elsewhere — the
//! focused-window-only model keeps the compositor's own
//! shortcuts working everywhere else (an "unwilling compositor"
//! can't happen while the user can still click away).

use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as ksim, zwp_keyboard_shortcuts_inhibitor_v1 as ksi,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

// Re-exports — mod.rs and globals.rs name these types here.
pub use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as KbdInhibitManager;
pub use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;

impl Dispatch<KbdInhibitManager, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &KbdInhibitManager,
        request: ksim::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ksim::Request::InhibitShortcuts { id, surface, .. } => {
                let obj = init.init::<ZwpKeyboardShortcutsInhibitorV1, wl_surface::WlSurface>(
                    id,
                    surface.clone(),
                );
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.kbd_inhibitor.lock().unwrap() = Some(obj.clone());
                    // Already focused → activate immediately; the
                    // client must learn its inhibition state
                    // without waiting for a focus hop.
                    if state
                        .kbd_focus
                        .as_ref()
                        .is_some_and(|f| f.id() == surface.id())
                    {
                        obj.active();
                    }
                }
            }
            ksim::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        obj: &ZwpKeyboardShortcutsInhibitorV1,
        request: ksi::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ksi::Request::Destroy = request {
            // Detach so key_event stops treating the surface as
            // inhibited.
            if let Some(sd) = surf.data::<SurfaceData>() {
                let mut slot = sd.kbd_inhibitor.lock().unwrap();
                if slot.as_ref().is_some_and(|o| o.id() == obj.id()) {
                    *slot = None;
                }
            }
        }
    }
}

impl CompState {
    /// Is the keyboard-focus surface currently inhibiting? Gate
    /// for compositor bindings inside key_event.
    pub fn kbd_inhibited(&self) -> bool {
        self.kbd_focus
            .as_ref()
            .and_then(|f| f.data::<SurfaceData>())
            .is_some_and(|sd| {
                sd.kbd_inhibitor
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|o| o.is_alive())
            })
    }

    /// Focus transition → push active/inactive to every surface's
    /// inhibitor. Called from set_kbd_focus so the events track
    /// real focus, not request order.
    pub fn kbd_inhibit_focus(&mut self, new_focus: Option<&wl_surface::WlSurface>) {
        let notify = |surf: &wl_surface::WlSurface, on: bool| {
            let Some(sd) = surf.data::<SurfaceData>() else {
                return;
            };
            if let Some(obj) = sd
                .kbd_inhibitor
                .lock()
                .unwrap()
                .as_ref()
                .filter(|o| o.is_alive())
            {
                if on {
                    obj.active();
                } else {
                    obj.inactive();
                }
            }
        };
        if let Some(f) = new_focus {
            notify(f, true);
        }
        // The surface LOSING focus gets inactive — find it among
        // the tracked order (inhibitors live on SurfaceData).
        let old = self
            .order
            .iter()
            .filter(|s| {
                new_focus.is_none_or(|n| s.id() != n.id())
                    && s.data::<SurfaceData>()
                        .is_some_and(|sd| sd.kbd_inhibitor.lock().unwrap().is_some())
            })
            .cloned()
            .collect::<Vec<_>>();
        for s in old {
            notify(&s, false);
        }
    }
}
