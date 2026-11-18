// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_touch — multi-touch delivery to clients.
//!
//! A client binds the seat's touch object; each finger gets a
//! compositor-assigned slot id. `down` hit-tests the surface
//! under the point and grabs: that point's motion/up events
//! follow the grabbed surface even when the finger slides off
//! — matching wl_pointer implicit-grab semantics. `frame`
//! marks the end of a libinput event batch; `cancel` fires on
//! compositor-initiated breaks (grab takeover, lock, client
//! death) and also arrives from libinput on palm rejection.

use std::collections::HashSet;
use wayland_server::protocol::{wl_surface, wl_touch};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

/// One live touch point — the surface grabbed at `down` plus
/// the global→local offset the hit-test computed, so motion
/// lands in the same local frame even for subsurfaces.
/// `gx`/`gy` track the last global position — touch-armed
/// move/resize grabs position windows from it.
pub struct TouchPoint {
    pub surf: wl_surface::WlSurface,
    pub off_x: f64,
    pub off_y: f64,
    pub gx: f64,
    pub gy: f64,
}

impl Dispatch<wl_touch::WlTouch, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        t: &wl_touch::WlTouch,
        request: wl_touch::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // `release` (v3+) is the only request — drop the object.
        if let wl_touch::Request::Release = request {
            state.touch_objs.retain(|o| o.id() != t.id());
        }
    }
}

impl CompState {
    /// Touch objects owned by the same client as `surf` — events
    /// never cross client boundaries.
    fn touch_objs_of(&self, surf: &wl_surface::WlSurface) -> Vec<wl_touch::WlTouch> {
        let Some(client) = surf.client() else {
            return Vec::new();
        };
        self.touch_objs
            .iter()
            .filter(|o| o.is_alive() && o.client() == Some(client.clone()))
            .cloned()
            .collect()
    }

    /// Finger landed — hit-test, grab the surface for this slot,
    /// send down to the owning client's touch objects. A slot
    /// that's still down gets an implicit up first: hardware
    /// never re-does a live slot, but virtual input and odd
    /// stacks can — the previous surface must still hear the
    /// release or its sequence stays unterminated.
    pub fn touch_down(&mut self, slot: i32, gx: f64, gy: f64) {
        if self.touch_pts.contains_key(&slot) {
            self.touch_up(slot);
        }
        let Some((surf, sx, sy)) = self.hit(gx, gy) else {
            return;
        };
        let serial = self.next_serial_pub();
        let time = super::now_ms();
        for t in self.touch_objs_of(&surf) {
            t.down(serial, time, &surf, slot, sx, sy);
            self.touch_dirty.insert(t.id().protocol_id());
        }
        // Serial → slot: an xdg_toplevel.move/resize request
        // quoting this serial gets a touch-armed grab.
        self.touch_serials.insert(serial, slot);
        self.touch_pts.insert(
            slot,
            TouchPoint {
                surf,
                off_x: gx - sx,
                off_y: gy - sy,
                gx,
                gy,
            },
        );
    }

    /// Finger moved — the slot's grabbed surface gets motion in
    /// its local coordinates, reusing the offset `down` recorded.
    /// A touch-armed move/resize grab eats the motion instead —
    /// the same rule pointer grabs apply.
    pub fn touch_motion(&mut self, slot: i32, gx: f64, gy: f64) {
        if self.grab.as_ref().is_some_and(|g| g.touch == Some(slot)) {
            self.grab_motion(gx, gy);
            return;
        }
        let Some(pt) = self.touch_pts.get_mut(&slot) else {
            return;
        };
        let (sx, sy) = (gx - pt.off_x, gy - pt.off_y);
        pt.gx = gx;
        pt.gy = gy;
        let surf = pt.surf.clone();
        let time = super::now_ms();
        for t in self.touch_objs_of(&surf) {
            t.motion(time, slot, sx, sy);
            self.touch_dirty.insert(t.id().protocol_id());
        }
    }

    /// Finger lifted — up, then the slot frees. Frame is sent
    /// by the caller's touch_frame after the event batch.
    pub fn touch_up(&mut self, slot: i32) {
        // A touch-armed move/resize grab releases on the finger
        // lift — the pointer never sends a button release for it.
        if self.grab.as_ref().is_some_and(|g| g.touch == Some(slot)) {
            self.grab_end();
        }
        // Serials keep their mapping after up — a client's
        // move/resize request can legitimately quote a serial
        // whose finger just lifted; the arm site checks slot
        // liveness. Bound the history so it can't grow forever.
        while self.touch_serials.len() > 256 {
            if let Some((&oldest, _)) = self.touch_serials.iter().next() {
                self.touch_serials.remove(&oldest);
            }
        }
        let Some(pt) = self.touch_pts.remove(&slot) else {
            return;
        };
        let serial = self.next_serial_pub();
        let time = super::now_ms();
        for t in self.touch_objs_of(&pt.surf) {
            t.up(serial, time, slot);
            self.touch_dirty.insert(t.id().protocol_id());
        }
    }

    /// Everything cancelled — compositor or libinput break.
    /// Every client holding a live point gets cancel+frame.
    pub fn touch_cancel(&mut self) {
        if self.touch_pts.is_empty() {
            return;
        }
        self.touch_pts.clear();
        self.touch_serials.clear();
        // Every touch-armed grab breaks with the cancel — no
        // finger lift will ever come to release it.
        if self.grab.as_ref().is_some_and(|g| g.touch.is_some()) {
            self.grab_end();
        }
        self.touch_objs.retain(|o| o.is_alive());
        for t in &self.touch_objs {
            t.cancel();
            self.touch_dirty.insert(t.id().protocol_id());
        }
        self.touch_frame();
    }

    /// End of an event batch — frame to every touch object that
    /// received events since the last frame.
    pub fn touch_frame(&mut self) {
        if self.touch_dirty.is_empty() {
            return;
        }
        let dirty: HashSet<u32> = std::mem::take(&mut self.touch_dirty);
        for t in self
            .touch_objs
            .iter()
            .filter(|o| dirty.contains(&o.id().protocol_id()))
        {
            t.frame();
        }
    }

    /// Drop every touch point still grabbed on a dead surface —
    /// called from the client-prune path so slots never leak.
    pub fn touch_prune(&mut self) {
        self.touch_pts.retain(|_, p| p.surf.is_alive());
    }
}
