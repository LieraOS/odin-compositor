// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_seat — pointer, keyboard and all focus/grab machinery.
//!
//! Focus model: pointer focus is sloppy (follows the cursor,
//! drives enter/leave/motion/button/axis), keyboard focus is
//! sticky (click or explicit action). Grabs (window move/resize)
//! swallow events while armed. All event times are real ms.

use std::time::Instant;
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::protocol::{wl_keyboard, wl_pointer, wl_seat, wl_surface, wl_touch};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{keymap::mods_to_xkb, now_ms, CompState, SurfaceData};

/// An active pointer grab — window move or resize. While armed,
/// pointer motion repositions/resizes the grabbed surface instead
/// of reaching clients; release disarms.
pub struct Grab {
    pub surf: wl_surface::WlSurface,
    pub kind: GrabKind,
    /// Touch slot that armed this grab — `Some` means it was
    /// initiated by wl_touch.down (touch-drag window move) and
    /// releases on that slot's up, not a pointer button.
    pub touch: Option<i32>,
}

pub enum GrabKind {
    /// Window move — `dx/dy` is the press offset inside the window,
    /// so the grab doesn't jump the titlebar to the cursor.
    Move { dx: f64, dy: f64 },
    /// Window resize — which edges the client asked us to pull,
    /// the rect at grab start, and the press point deltas are
    /// computed against.
    Resize {
        edges: xdg_toplevel::ResizeEdge,
        start: (i32, i32, i32, i32),
        anchor: (f64, f64),
    },
}

impl wayland_server::GlobalDispatch<wl_seat::WlSeat, ()> for CompState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<wl_seat::WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init::<wl_seat::WlSeat, ()>(resource, ());
        seat.capabilities(
            wl_seat::Capability::Pointer
                | wl_seat::Capability::Keyboard
                | wl_seat::Capability::Touch,
        );
        info!("seat bound — capabilities announced");
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                let p = init.init::<wl_pointer::WlPointer, ()>(id, ());
                state.pointers.push(p);
            }
            wl_seat::Request::GetTouch { id } => {
                let t = init.init::<wl_touch::WlTouch, ()>(id, ());
                state.touch_objs.push(t);
            }
            wl_seat::Request::GetKeyboard { id } => {
                let kb = init.init::<wl_keyboard::WlKeyboard, ()>(id, ());
                state.keyboards.push(kb.clone());
                if let Some((fd, size)) = super::keymap::keymap_fd(&state.cfg) {
                    use std::os::fd::AsFd;
                    kb.keymap(wl_keyboard::KeymapFormat::XkbV1, fd.as_fd(), size);
                    // 25 repeats/sec after 400ms — the GNOME/KDE
                    // defaults. repeat_info exists only since
                    // wl_keyboard v4 — sending it to an older bind
                    // is a protocol error that kills the client.
                    if kb.version() >= 4 {
                        kb.repeat_info(25, 400);
                    }
                    info!("keymap + repeat info sent to client");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for CompState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &wl_pointer::WlPointer,
        request: wl_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_pointer::Request::SetCursor {
            surface,
            hotspot_x,
            hotspot_y,
            ..
        } = request
        {
            // Only the client under the pointer may pick the
            // cursor — the protocol makes set_cursor valid only
            // while the pointer is over a surface of the
            // requesting client. Without the gate a background
            // client could hijack the pointer image.
            if state
                .focus
                .as_ref()
                .is_none_or(|f| !state.client_matches(f, Some(client.clone())))
            {
                return;
            }
            if let Some(surf) = &surface {
                if let Some(sd) = surf.data::<SurfaceData>() {
                    sd.is_cursor
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                // The client may have committed the cursor image
                // before declaring the role — it would sit in the
                // stacking order as a frozen ghost at its position.
                state.ft_closed(surf);
                state.order.retain(|s| s.id() != surf.id());
            }
            // Only the cursor pixels change — 128px covers any
            // client cursor surface at the pointer spot.
            let (px, py) = (state.ptr_pos.0 as i32, state.ptr_pos.1 as i32);
            state.damage.push((px - 64, py - 64, 128, 128));
            state.dirty = true;
            state.cursor_surf = surface;
            state.cursor_hotspot = (hotspot_x, hotspot_y);
            // Last request wins — an explicit cursor surface
            // overrides a previously set wp_cursor_shape image.
            state.cursor_img = None;
            // Hardware cursor planes re-upload on the bump.
            state.cursor_gen += 1;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_keyboard::WlKeyboard,
        _: wl_keyboard::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl CompState {
    /// Raise a surface to the top of the stacking order.
    pub(crate) fn raise(&mut self, surf: &wl_surface::WlSurface) {
        if let Some(idx) = self.order.iter().position(|s| s.id() == surf.id()) {
            let s = self.order.remove(idx);
            self.order.push(s);
            // A raised parent pulls its transients up with it.
            self.restack_transients();
            self.damage_everything();
            // X windows mirror the raise on their side — Xwayland
            // tracks stacking for its own coordinate math.
            if let Some(wid) = surf
                .data::<SurfaceData>()
                .and_then(|sd| *sd.xwin.lock().unwrap())
            {
                self.xreqs.push(crate::xwm::XReq::Raise(wid));
            }
        }
    }

    /// Keyboard focus transfer — sends leave/enter+modifiers and
    /// updates toplevel activated state so clients render their
    /// titlebars right. Also pushes the current clipboard offer
    /// to the newly focused client (the Wayland selection model:
    /// the focused client owns the paste target).
    pub fn set_kbd_focus(&mut self, surf: Option<wl_surface::WlSurface>) {
        let same = match (&self.kbd_focus, &surf) {
            (Some(a), Some(b)) => a.id() == b.id(),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        // xdg_dialog_v1 — a modal dialog owns interaction for its
        // whole ancestor chain: focusing a parent while its modal
        // child lives lands on the dialog instead.
        let surf = surf.map(|s| self.modal_descendant(&s).unwrap_or(s));
        let serial = self.next_serial_pub();
        if let Some(old) = self.kbd_focus.take() {
            if old.is_alive() {
                for kb in self.keyboards.clone() {
                    if self.client_matches(&old, kb.client()) {
                        kb.leave(serial, &old);
                    }
                }
                self.set_tl_state(&old, xdg_toplevel::State::Activated, false);
            }
        }
        if let Some(new) = &surf {
            let (dep, lat, lck) = mods_to_xkb(self.udev_mods);
            for kb in self.keyboards.clone() {
                if self.client_matches(new, kb.client()) {
                    kb.enter(serial, new, Vec::new());
                    kb.modifiers(serial, dep, lat, lck, 0);
                }
            }
            self.set_tl_state(new, xdg_toplevel::State::Activated, true);
            // Selection follows keyboard focus — the standard
            // compositor behavior every toolkit expects.
            self.push_selection(new);
            // Primary selection rides the same focus rule —
            // middle-click paste needs it too.
            self.push_primary(new);
        }
        self.kbd_focus = surf;
        // Inhibitor state tracks focus — the newly focused
        // surface's inhibitor goes active, every other goes
        // inactive.
        let nf = self.kbd_focus.clone();
        self.kbd_inhibit_focus(nf.as_ref());
        // Text-input enter/leave — feeds enabled fields to the
        // input method.
        self.ti_focus(nf.as_ref());
        // EWMH mirror — X11 clients track _NET_ACTIVE_WINDOW and
        // need SetInputFocus to receive key events. Focusing a
        // plain Wayland window (or nothing) clears the X focus.
        let xw = self
            .kbd_focus
            .as_ref()
            .and_then(|s| s.data::<SurfaceData>())
            .and_then(|sd| *sd.xwin.lock().unwrap());
        self.xreqs.push(crate::xwm::XReq::Activate(xw.unwrap_or(0)));
        // Taskbar handles mirror the Activated bit — refresh all
        // of them since only the focus target knows nothing moved.
        self.ft_refresh_focus();
        self.damage_everything();
    }

    /// Add/remove a toplevel state and reconfigure — clients draw
    /// their focused/decorated look off Activated.
    pub fn set_tl_state(
        &mut self,
        surf: &wl_surface::WlSurface,
        st: xdg_toplevel::State,
        on: bool,
    ) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        let mut states = sd.tl_states.lock().unwrap().clone();
        if on && !states.contains(&st) {
            states.push(st);
        } else if !on {
            states.retain(|s| *s != st);
        } else {
            return;
        }
        *sd.tl_states.lock().unwrap() = states;
        crate::compositor::xdg::configure_tl(self, sd, 0, 0);
    }

    /// wl_keyboard.key to the sticky focus — repeat_tick and the
    /// native input path share this sender.
    pub fn send_key(&mut self, key: u32, pressed: bool) {
        self.prune_dead();
        // A live input-method grab eats every key before the
        // focus surface sees it — the IM composes the text.
        if self.im_key(key, pressed) {
            return;
        }
        let focus = self.kbd_focus.clone();
        if let Some(surf) = focus {
            let serial = self.next_serial_pub();
            let st = if pressed {
                wl_keyboard::KeyState::Pressed
            } else {
                wl_keyboard::KeyState::Released
            };
            for kb in self.keyboards.clone() {
                if self.client_matches(&surf, kb.client()) {
                    kb.key(serial, now_ms(), key, st);
                }
            }
        }
    }

    /// Modifier mask changed → wl_keyboard.modifiers to the focus.
    /// Clients need this to evaluate Ctrl+C/Shift+click themselves.
    pub(crate) fn push_modifiers(&mut self) {
        if self.im_modifiers() {
            return;
        }
        let Some(surf) = self.kbd_focus.clone() else {
            return;
        };
        let serial = self.next_serial_pub();
        let (dep, lat, lck) = mods_to_xkb(self.udev_mods);
        for kb in self.keyboards.clone() {
            if self.client_matches(&surf, kb.client()) {
                kb.modifiers(serial, dep, lat, lck, 0);
            }
        }
    }

    /// Relative pointer motion from libinput — `dx`/`dy` are the
    /// accelerated deltas that move the cursor, `ux`/`uy` the raw
    /// ones relative-pointer clients (games) consume. Constraints
    /// run on the absolute result, never on the raw stream.
    pub fn ptr_rel(&mut self, dx: f64, dy: f64, ux: f64, uy: f64) {
        self.last_input = Instant::now();
        // Relative-pointer deltas go out first — they must flow
        // even while a lock freezes the visible cursor.
        self.rel_motion(dx, dy, ux, uy);
        if self.locked() {
            self.dirty = true;
            return;
        }
        // During a grab the cursor still moves (it visually drags
        // the window edge); clamp against space, then confine.
        let (nx, ny) = (
            (self.ptr_pos.0 + dx).clamp(0.0, self.space.0 - 1.0),
            (self.ptr_pos.1 + dy).clamp(0.0, self.space.1 - 1.0),
        );
        let (nx, ny) = self.apply_confine(nx, ny);
        self.ptr_motion(nx, ny);
    }
}
impl CompState {
    /// Drop resources whose owning client disconnected — posting
    /// events on dead objects is at best a no-op, and passing a
    /// dead surface's object id to ANOTHER client's wl_pointer.enter
    /// is a hard protocol error that kills that client.
    /// A freshly created wl_surface may close a pending X
    /// association — the WL_SURFACE_ID message travels the X
    /// socket while the surface create travels the Wayland fd,
    /// so the two race. If the new surface belongs to Xwayland
    /// and a recorded surface id matches, bind it now.
    pub(crate) fn bind_pending_xwin(&mut self, surf: &wl_surface::WlSurface) {
        let Some(xc) = &self.xwl_client else { return };
        if surf.client().map(|c| c.id() != xc.id()).unwrap_or(true) {
            return;
        }
        let sid = surf.id().protocol_id();
        let wid = self
            .xwins
            .iter()
            .find(|(_, xw)| !xw.associated && xw.surface_id == Some(sid))
            .map(|(w, _)| *w);
        if let Some(wid) = wid {
            crate::xwm::assoc::try_bind(self, wid);
        }
    }
}
