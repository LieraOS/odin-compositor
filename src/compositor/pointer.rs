// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pointer path — hit-testing, focus enter/leave, motion,
//! buttons, axis and the window grabs that swallow them.
//!
//! Hit order: popups first (menus float above windows), then
//! toplevels top→bottom. Clicking a popup focuses its parent
//! window — the menu itself is not a focusable toplevel.

use wayland_server::protocol::wl_pointer;
use wayland_server::Resource;

use super::{now_ms, CompState, SurfaceData};

impl CompState {
    /// Pointer focus evaluation — enter/leave fire only on an
    /// actual change, and only to the surface-owning client's
    /// objects (foreign ids in enter are a protocol error).
    pub fn ptr_enter(&mut self, x: f64, y: f64) {
        self.prune_dead();
        let top = self.hit(x, y);
        self.ptr_inside = true;
        let changed = match (&self.focus, &top) {
            (Some(a), Some((b, _, _))) => a.id() != b.id(),
            (None, None) => false,
            _ => true,
        };
        if changed {
            if let Some(old) = self.focus.take() {
                let serial = self.next_serial_pub();
                for p in self.pointers.clone() {
                    if self.client_matches(&old, p.client()) {
                        p.leave(serial, &old);
                        if p.version() >= 5 {
                            p.frame();
                        }
                    }
                }
            }
            if let Some((new, lx, ly)) = &top {
                let serial = self.next_serial_pub();
                let (pos, geom) = new
                    .data::<SurfaceData>()
                    .map(|sd| (*sd.pos.lock().unwrap(), *sd.win_geom.lock().unwrap()))
                    .unwrap_or(((0, 0), (0, 0, 0, 0)));
                tracing::debug!(
                    surf = new.id().protocol_id(),
                    lx,
                    ly,
                    ?pos,
                    ?geom,
                    x = self.ptr_pos.0,
                    y = self.ptr_pos.1,
                    "pointer enter dispatched"
                );
                for p in self.pointers.clone() {
                    if self.client_matches(new, p.client()) {
                        p.enter(serial, new, *lx, *ly);
                        if p.version() >= 5 {
                            p.frame();
                        }
                    }
                }
            }
            self.focus = top.map(|(s, _, _)| s);
            // The previous surface's cursor is void on a focus
            // change — the entered client re-arms its own via
            // set_cursor/set_shape right after `enter`. Without
            // this the last shape (resize grip, dnd feedback)
            // stayed stuck over surfaces that never set one —
            // including the desktop itself.
            if self.cursor_surf.is_some() || self.cursor_img.is_some() {
                self.cursor_surf = None;
                self.cursor_img = None;
                self.cursor_gen += 1;
                let (px, py) = (x as i32, y as i32);
                self.damage.push((px - 64, py - 64, 128, 128));
                self.dirty = true;
            }
            // Focus transitions arm/disarm pointer constraints —
            // a game window re-locks when the cursor re-enters it.
            self.update_constraint();
        }
    }

    /// Absolute pointer motion — grabs swallow it, otherwise it
    /// re-evaluates focus and forwards surface-local motion.
    /// Absolute devices (tablets, nested backend) synthesize the
    /// delta for relative-pointer clients from the position jump.
    pub fn ptr_motion(&mut self, x: f64, y: f64) {
        self.last_input = std::time::Instant::now();
        // A software cursor rides the composite — the old and new
        // spots are damage (128px covers any client cursor
        // surface plus a drag icon riding the pointer). On a
        // hardware plane the move is a bare ioctl: no damage,
        // no render, the scene doesn't even wake for it.
        if self.sw_cursor {
            let (ox, oy) = (self.ptr_pos.0 as i32, self.ptr_pos.1 as i32);
            let (nx0, ny0) = (x as i32, y as i32);
            self.damage.push((ox - 64, oy - 64, 128, 128));
            self.damage.push((nx0 - 64, ny0 - 64, 128, 128));
            self.dirty = true;
        }
        let dx = x - self.ptr_pos.0;
        let dy = y - self.ptr_pos.1;
        // Relative deltas accumulate — dispatched once per frame
        // by flush_motion, keeping the client's view identical
        // while bounding its event rate to the refresh.
        self.rel_pending.0 += dx;
        self.rel_pending.1 += dy;
        self.rel_pending.2 += dx;
        self.rel_pending.3 += dy;
        if self.locked() {
            return;
        }
        let (x, y) = self.apply_confine(x, y);
        self.ptr_pos = (x, y);
        // An active drag is an implicit grab — pointer events
        // stay silent; only drag events reach the destination.
        if self.drag.is_some() {
            self.drag_motion(x, y);
            return;
        }
        if self.grab_motion(x, y) {
            return;
        }
        // Defer the client dispatch — flush_motion folds every
        // raw event of this frame into a single motion.
        self.motion_pending = true;
    }

    /// Deliver the coalesced pointer state — one enter/motion per
    /// frame (and one accumulated relative_motion) instead of one
    /// per raw libinput event. Called at the frame tick and before
    /// any ordered pointer event (button, axis) so event order is
    /// preserved.
    pub fn flush_motion(&mut self) {
        // Coalesced resize configure — one per frame tick while
        // the grab drags, never per raw motion event.
        if let Some((surf, w, h)) = self.rs_pending.take() {
            if surf.is_alive() {
                if let Some(sd) = surf.data::<SurfaceData>() {
                    let prev = sd.tl_states.lock().unwrap().clone();
                    *sd.tl_states.lock().unwrap() =
                        vec![wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing];
                    crate::compositor::xdg::configure_tl(self, sd, w, h);
                    *sd.tl_states.lock().unwrap() = prev;
                }
            }
        }
        let (dx, dy, ux, uy) = std::mem::take(&mut self.rel_pending);
        if dx != 0.0 || dy != 0.0 || ux != 0.0 || uy != 0.0 {
            self.rel_motion(dx, dy, ux, uy);
        }
        if !self.motion_pending {
            return;
        }
        self.motion_pending = false;
        let (x, y) = self.ptr_pos;
        self.ptr_enter(x, y);
        if let Some(surf) = self.focus.clone() {
            let (ox, oy) = self.local_origin(&surf);
            for p in self.pointers.clone() {
                if self.client_matches(&surf, p.client()) {
                    p.motion(now_ms(), x - ox, y - oy);
                    if p.version() >= 5 {
                        p.frame();
                    }
                }
            }
        }
    }
    pub fn ptr_button(&mut self, button: u32, pressed: bool) {
        self.last_input = std::time::Instant::now();
        // The click belongs to the position where it happened —
        // dispatch pending motion first so enter/motion precede
        // the button on the wire.
        self.flush_motion();
        // Track held buttons — start_drag is only legal with a
        // button down (the implicit-grab rule of the protocol).
        if pressed {
            self.ptr_down += 1;
        } else {
            self.ptr_down = self.ptr_down.saturating_sub(1);
        }
        // A button release during a drag IS the drop — the
        // pointer never sees it as a click.
        if self.drag.is_some() {
            if !pressed {
                self.drag_drop();
            }
            return;
        }
        if !pressed {
            if self.grab.is_some() {
                // Sends the resize's final non-Resizing configure.
                self.grab_end();
                return; // the release that ended a grab is swallowed
            }
        }
        if pressed {
            let (x, y) = self.ptr_pos;
            // Click outside all popups → dismiss them.
            let inside_popup = self
                .hit(x, y)
                .map(|(s, _, _)| self.popups.iter().any(|p| p.surf.id() == s.id()))
                .unwrap_or(false);
            if !inside_popup && !self.popups.is_empty() {
                // Popups leave the scene — repaint their
                // footprints (global = parent pos + rel offset),
                // the rest of the frame is untouched.
                for p in &self.popups {
                    p.obj.popup_done();
                    let (px, py) = p
                        .parent
                        .data::<SurfaceData>()
                        .map(|sd| *sd.pos.lock().unwrap())
                        .unwrap_or((0, 0));
                    self.damage
                        .push((px + p.rel.0, py + p.rel.1, p.size.0, p.size.1));
                }
                self.popups.clear();
            }
            // Compositor chrome first — an SSD titlebar/border
            // click belongs to the compositor's frame, never to
            // the client surface inside it.
            if self.ssd_button(x, y) {
                return;
            }
            if let Some((surf, lx, ly)) = self.hit_window(x, y) {
                // Input forensics — which surface class claims the
                // press. A click that dies before its window shows
                // up here as the wrong class (overlay layer,
                // override-redirect surface, foreign client).
                let class = if self.xpopups.iter().any(|s| s.id() == surf.id()) {
                    "xpopup"
                } else if self.popups.iter().any(|p| p.surf.id() == surf.id()) {
                    "popup"
                } else if self.layer_index(&surf).is_some() {
                    "layer"
                } else if self.order.iter().any(|s| s.id() == surf.id()) {
                    "toplevel"
                } else {
                    "other"
                };
                let matched = self
                    .pointers
                    .iter()
                    .filter(|p| self.client_matches(&surf, p.client()))
                    .count();
                tracing::debug!(
                    x,
                    y,
                    lx,
                    ly,
                    class,
                    surf = surf.id().protocol_id(),
                    total = self.pointers.len(),
                    matched,
                    "pointer press target"
                );
                if self.session_locked() {
                    // Lock surfaces get focus but never enter the
                    // stacking order — raise() would leak them
                    // into `order` and tile them after unlock.
                    self.set_kbd_focus(Some(surf));
                } else if self.xpopups.iter().any(|s| s.id() == surf.id()) {
                    // X11 override-redirect windows get the click
                    // (pointer focus already delivers it), but
                    // never keyboard focus or a raise — a
                    // SetInputFocus here would fire FocusOut at
                    // the owning window and roll the popup
                    // (menus, doorhangers) closed instantly.
                } else if self.layer_index(&surf).is_some() {
                    // Layer surfaces never raise — they're not in the
                    // window stack — and only take keyboard focus when
                    // they asked for it (panels: none, launchers:
                    // exclusive/on_demand).
                    if self.layer_wants_kbd(&surf) {
                        self.set_kbd_focus(Some(surf));
                    }
                } else {
                    self.raise(&surf);
                    self.set_kbd_focus(Some(surf));
                }
            }
        }
        if let Some(surf) = self.focus.clone() {
            let serial = self.next_serial_pub();
            let st = if pressed {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            };
            for p in self.pointers.clone() {
                if self.client_matches(&surf, p.client()) {
                    p.button(serial, now_ms(), button, st);
                    if p.version() >= 5 {
                        p.frame();
                    }
                }
            }
        }
    }

    /// Scroll — libinput pointer axis → wl_pointer.axis.
    /// `axis`: 0 = vertical, 1 = horizontal. `source` is the
    /// wl_pointer::AxisSource discriminant — finger-driven
    /// touchpad scroll MUST be tagged Finger or GTK applies
    /// wheel semantics (steps instead of smooth scroll).
    pub fn ptr_axis(&mut self, axis: u32, value: f64, source: u32) {
        self.last_input = std::time::Instant::now();
        // Same ordering rule as buttons — scroll events land at
        // the position the pointer had when they fired.
        self.flush_motion();
        self.dirty = true;
        let Some(surf) = self.focus.clone() else {
            return;
        };
        let ax = if axis == 0 {
            wl_pointer::Axis::VerticalScroll
        } else {
            wl_pointer::Axis::HorizontalScroll
        };
        let src = match source {
            1 => wl_pointer::AxisSource::Finger,
            2 => wl_pointer::AxisSource::Continuous,
            _ => wl_pointer::AxisSource::Wheel,
        };
        for p in self.pointers.clone() {
            if self.client_matches(&surf, p.client()) {
                p.axis(now_ms(), ax, value);
                if p.version() >= 5 {
                    p.axis_source(src);
                    p.frame();
                }
            }
        }
    }
}
