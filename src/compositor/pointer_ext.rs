// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pointer protocol extensions — the three protocols that make
//! pointer input behave correctly for games and precision work:
//!
//! - zwp_relative_pointer_manager_v1: raw (unaccelerated) deltas
//!   next to the accelerated ones — FPS camera controls live off
//!   this; without it a game only sees the clamped absolute
//!   position and the cursor feels like it "sticks" at edges.
//! - zwp_pointer_constraints_v1: lock (cursor frozen, deltas
//!   still flow) and confine (cursor kept inside a region).
//! - wp_cursor_shape_manager_v1: the client names a shape, the
//!   compositor draws it from the system Xcursor theme — the
//!   hotspot lives inside the image so clicks land exactly where
//!   the cursor visually points.

use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_confined_pointer_v1 as cp;
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_locked_pointer_v1 as lp;
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_pointer_constraints_v1 as pc;
use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_manager_v1 as rpm;
use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_v1 as rp;
// Re-exported — CompState field types and the gs! global binds
// in mod.rs name them through this module's path.
pub use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
pub use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_locked_pointer_v1::ZwpLockedPointerV1;
pub use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;
pub use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
pub use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_server::protocol::{wl_region, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::{CompState, SurfaceData};
use crate::compositor::shm::RegionData;

/// Microsecond timestamp for relative_motion — the protocol
/// carries a 64-bit utime split into hi/lo words.
pub(crate) fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// One live pointer constraint — at most one exists seat-wide;
/// a second request replaces the first (the spec's
/// `already_constrained` error is reserved for a still-active
/// constraint on the same surface).
pub struct Constraint {
    /// Surface the constraint belongs to — it only bites while
    /// this surface holds pointer focus.
    pub surf: wl_surface::WlSurface,
    /// Whether the constraint is currently activated.
    pub active: bool,
    /// Persistent constraints re-arm when focus returns; one-shot
    /// ones are dropped after the first deactivation.
    pub persistent: bool,
    pub kind: ConstraintKind,
    /// Region rects (x, y, w, h) in surface-local coordinates —
    /// empty means the whole surface. Only confine uses it.
    pub region: Vec<(i32, i32, i32, i32)>,
    /// Position hint from set_cursor_position_hint — where the
    /// cursor should land when the lock deactivates.
    pub unlock_hint: Option<(f64, f64)>,
}

pub enum ConstraintKind {
    Lock(ZwpLockedPointerV1),
    Confine(ZwpConfinedPointerV1),
}

/// Rect list carried by a wl_region — confine regions arrive as
/// protocol objects, resolve them into plain rectangles.
fn region_rects(region: &Option<wl_region::WlRegion>) -> Vec<(i32, i32, i32, i32)> {
    region
        .as_ref()
        .and_then(|r| r.data::<RegionData>())
        .map(|d| d.rects.lock().unwrap().clone())
        .unwrap_or_default()
}

/// Closest point to (x, y) still inside the union of rects —
/// used to clamp a confined pointer without warping it.
fn confine_point(x: f64, y: f64, rects: &[(i32, i32, i32, i32)]) -> (f64, f64) {
    let inside = rects.iter().any(|&(rx, ry, rw, rh)| {
        x >= rx as f64 && x < (rx + rw) as f64 && y >= ry as f64 && y < (ry + rh) as f64
    });
    if inside || rects.is_empty() {
        return (x, y);
    }
    let mut best = (x, y);
    let mut best_d = f64::MAX;
    for &(rx, ry, rw, rh) in rects {
        let cx = x.clamp(rx as f64, (rx + rw) as f64 - 1.0);
        let cy = y.clamp(ry as f64, (ry + rh) as f64 - 1.0);
        let d = (cx - x).powi(2) + (cy - y).powi(2);
        if d < best_d {
            best_d = d;
            best = (cx, cy);
        }
    }
    best
}

impl CompState {
    /// Fan out one motion's deltas to every relative-pointer
    /// object of the client whose surface holds pointer focus.
    /// Games live off the unaccelerated pair — it must arrive
    /// even while the absolute position is locked or confined.
    pub(crate) fn rel_motion(&self, dx: f64, dy: f64, ux: f64, uy: f64) {
        let Some(foc) = &self.focus else { return };
        let t = now_us();
        for r in &self.rel_ptrs {
            if r.is_alive() && self.client_matches(foc, r.client()) {
                r.relative_motion((t >> 32) as u32, t as u32, dx, dy, ux, uy);
            }
        }
    }

    /// True while an active locked-pointer constraint freezes the
    /// cursor — motion events are swallowed, deltas still flow.
    pub(crate) fn locked(&self) -> bool {
        matches!(
            &self.constraint,
            Some(c) if c.active && matches!(c.kind, ConstraintKind::Lock(_))
        )
    }

    /// Global-space clamp rects for an active confine — empty
    /// region means the whole surface. Returns None when no
    /// confine is in force.
    pub(crate) fn confine_rects(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        let c = self.constraint.as_ref()?;
        if !c.active || !matches!(c.kind, ConstraintKind::Confine(_)) {
            return None;
        }
        let (ox, oy) = self.local_origin(&c.surf);
        if !c.region.is_empty() {
            return Some(
                c.region
                    .iter()
                    .map(|&(x, y, w, h)| (x + ox as i32, y + oy as i32, w, h))
                    .collect(),
            );
        }
        // Whole-surface region — use the window geometry so CSD
        // shadow margins don't count.
        let sd = c.surf.data::<SurfaceData>()?;
        let (bw, bh) = sd
            .shown_buf
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.data::<super::BufInfo>())
            .map(|bi| (bi.w, bi.h))
            .unwrap_or((0, 0));
        Some(vec![(ox as i32, oy as i32, bw, bh)])
    }

    /// Clamp a candidate position into the confine region —
    /// returns the input unchanged when nothing confines it.
    pub(crate) fn apply_confine(&self, x: f64, y: f64) -> (f64, f64) {
        match self.confine_rects() {
            Some(rects) => confine_point(x, y, &rects),
            None => (x, y),
        }
    }

    /// Constraint activation follows pointer focus: entering the
    /// constrained surface activates (locked/confined event),
    /// leaving deactivates (unlocked/unconfined) and applies the
    /// client's cursor hint — that last part is what makes a
    /// game's cursor reappear where it was instead of warping to
    /// a stale position.
    pub(crate) fn update_constraint(&mut self) {
        let Some(c) = &self.constraint else { return };
        let surf = c.surf.clone();
        let was_active = c.active;
        let persistent = c.persistent;
        let hint = c.unlock_hint;
        let on_focus = self
            .focus
            .as_ref()
            .map(|f| f.id() == surf.id())
            .unwrap_or(false);
        if on_focus && !was_active {
            tracing::info!("constraint activated — confined/locked sent");
            let c = self.constraint.as_mut().unwrap();
            c.active = true;
            match &c.kind {
                ConstraintKind::Lock(o) => o.locked(),
                ConstraintKind::Confine(o) => o.confined(),
            }
            return;
        }
        if !on_focus && was_active {
            {
                let c = self.constraint.as_mut().unwrap();
                c.active = false;
                match &c.kind {
                    ConstraintKind::Lock(o) => o.unlocked(),
                    ConstraintKind::Confine(o) => o.unconfined(),
                }
            }
            if let Some((hx, hy)) = hint {
                let (ox, oy) = self.local_origin(&surf);
                let (nx0, ny0) = (
                    (ox + hx).clamp(0.0, self.space.0 - 1.0),
                    (oy + hy).clamp(0.0, self.space.1 - 1.0),
                );
                self.damage.push((
                    self.ptr_pos.0 as i32 - 64,
                    self.ptr_pos.1 as i32 - 64,
                    128,
                    128,
                ));
                self.damage
                    .push((nx0 as i32 - 64, ny0 as i32 - 64, 128, 128));
                self.ptr_pos = (nx0, ny0);
                self.dirty = true;
            }
            if !persistent {
                self.constraint = None;
            }
        }
    }
}

// ── relative pointer ───────────────────────────────────────────

impl Dispatch<ZwpRelativePointerManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpRelativePointerManagerV1,
        request: rpm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let rpm::Request::GetRelativePointer { id, .. } = request {
            state
                .rel_ptrs
                .push(init.init::<ZwpRelativePointerV1, ()>(id, ()));
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwpRelativePointerV1,
        request: rp::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let rp::Request::Destroy = request {
            state.rel_ptrs.retain(|o| o.id() != obj.id());
        }
    }
}

// ── pointer constraints ────────────────────────────────────────

impl Dispatch<ZwpPointerConstraintsV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPointerConstraintsV1,
        request: pc::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            pc::Request::LockPointer {
                id,
                surface,
                region,
                lifetime,
                ..
            } => {
                let obj = init.init::<ZwpLockedPointerV1, ()>(id, ());
                state.constraint = Some(Constraint {
                    surf: surface,
                    active: false,
                    persistent: matches!(lifetime, WEnum::Value(pc::Lifetime::Persistent)),
                    kind: ConstraintKind::Lock(obj),
                    region: region_rects(&region),
                    unlock_hint: None,
                });
                state.update_constraint();
            }
            pc::Request::ConfinePointer {
                id,
                surface,
                region,
                lifetime,
                ..
            } => {
                let obj = init.init::<ZwpConfinedPointerV1, ()>(id, ());
                state.constraint = Some(Constraint {
                    surf: surface,
                    active: false,
                    persistent: matches!(lifetime, WEnum::Value(pc::Lifetime::Persistent)),
                    kind: ConstraintKind::Confine(obj),
                    region: region_rects(&region),
                    unlock_hint: None,
                });
                state.update_constraint();
            }
            pc::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwpLockedPointerV1,
        request: lp::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            lp::Request::SetCursorPositionHint {
                surface_x,
                surface_y,
            } => {
                if let Some(c) = &mut state.constraint {
                    c.unlock_hint = Some((surface_x, surface_y));
                }
            }
            lp::Request::SetRegion { region } => {
                if let Some(c) = &mut state.constraint {
                    c.region = region_rects(&region);
                }
            }
            lp::Request::Destroy => {
                if let Some(c) = &state.constraint {
                    if matches!(&c.kind, ConstraintKind::Lock(o) if o.id() == obj.id()) {
                        state.constraint = None;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwpConfinedPointerV1,
        request: cp::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            cp::Request::SetRegion { region } => {
                if let Some(c) = &mut state.constraint {
                    c.region = region_rects(&region);
                }
            }
            cp::Request::Destroy => {
                if let Some(c) = &state.constraint {
                    if matches!(&c.kind, ConstraintKind::Confine(o) if o.id() == obj.id()) {
                        state.constraint = None;
                    }
                }
            }
            _ => {}
        }
    }
}
