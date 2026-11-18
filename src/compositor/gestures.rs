// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_pointer_gestures_v1 — touchpad gestures delivered to
//! clients: swipe (scroll/navigation), pinch (zoom/rotate) and
//! hold. libinput already produces these on real touchpads; this
//! module owns the wire objects and the focus-gated delivery.
//!
//! Each gesture object is created FOR a specific wl_pointer —
//! events fan out only to objects whose pointer belongs to the
//! client holding pointer focus, the same rule every other
//! pointer event follows.

use wayland_protocols::wp::pointer_gestures::zv1::server::{
    zwp_pointer_gesture_hold_v1 as ghold, zwp_pointer_gesture_pinch_v1 as gpinch,
    zwp_pointer_gesture_swipe_v1 as gswipe, zwp_pointer_gestures_v1 as gest,
};
use wayland_server::protocol::{wl_pointer, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{now_ms, CompState};

// Re-exports — mod.rs and globals.rs name these types through
// this path.
pub use wayland_protocols::wp::pointer_gestures::zv1::server::zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1;
pub use wayland_protocols::wp::pointer_gestures::zv1::server::zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1;
pub use wayland_protocols::wp::pointer_gestures::zv1::server::zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1;
pub use wayland_protocols::wp::pointer_gestures::zv1::server::zwp_pointer_gestures_v1::ZwpPointerGesturesV1;

/// Gesture lifecycle phases — the three events every gesture
/// type shares; `End` additionally carries `cancelled`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Begin,
    Update,
    End,
}

impl Dispatch<ZwpPointerGesturesV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPointerGesturesV1,
        request: gest::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            gest::Request::GetSwipeGesture { id, pointer } => {
                let obj = init.init::<ZwpPointerGestureSwipeV1, wl_pointer::WlPointer>(id, pointer);
                state.swipe_gestures.push(obj);
            }
            gest::Request::GetPinchGesture { id, pointer } => {
                let obj = init.init::<ZwpPointerGesturePinchV1, wl_pointer::WlPointer>(id, pointer);
                state.pinch_gestures.push(obj);
            }
            gest::Request::GetHoldGesture { id, pointer } => {
                let obj = init.init::<ZwpPointerGestureHoldV1, wl_pointer::WlPointer>(id, pointer);
                state.hold_gestures.push(obj);
            }
            gest::Request::Release => {}
            _ => {}
        }
    }
}

// The gesture objects take only `destroy` — every request
// reaching these impls is one. Drop the object out of the
// delivery vec so a stale one can't leak into a later gesture
// stream.
macro_rules! gesture_dispatch {
    ($t:ty, $m:ty, $field:ident) => {
        impl Dispatch<$t, wl_pointer::WlPointer> for CompState {
            fn request(
                state: &mut Self,
                _: &Client,
                obj: &$t,
                _request: $m,
                _: &wl_pointer::WlPointer,
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
                state.$field.retain(|o| o.id() != obj.id());
            }
        }
    };
}

gesture_dispatch!(ZwpPointerGestureSwipeV1, gswipe::Request, swipe_gestures);
gesture_dispatch!(ZwpPointerGesturePinchV1, gpinch::Request, pinch_gestures);
gesture_dispatch!(ZwpPointerGestureHoldV1, ghold::Request, hold_gestures);

/// Does `ptr`'s owning client currently hold the pointer focus?
/// Gesture events belong to the focused surface's client.
fn owns_focus(
    _state: &CompState,
    ptr: &wl_pointer::WlPointer,
    focus: &wl_surface::WlSurface,
) -> bool {
    match (ptr.client(), focus.client()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

impl CompState {
    /// Touchpad swipe — begin/update/end with per-update deltas
    /// in surface coordinate space. Delivers to the swipe
    /// objects of the pointer-focus client only.
    pub fn swipe(&mut self, phase: Phase, dx: f64, dy: f64, fingers: u32, cancelled: bool) {
        let Some(focus) = self.focus.clone() else {
            return;
        };
        let (serial, time) = (self.next_serial_pub(), now_ms());
        self.swipe_gestures.retain(|o| o.is_alive());
        for obj in &self.swipe_gestures {
            // Object udata is the wl_pointer it was created for.
            let ok = obj
                .data::<wl_pointer::WlPointer>()
                .is_some_and(|p| owns_focus(self, p, &focus));
            if !ok {
                continue;
            }
            match phase {
                Phase::Begin => obj.begin(serial, time, &focus, fingers),
                Phase::Update => obj.update(time, dx, dy),
                Phase::End => obj.end(serial, time, cancelled as i32),
            }
        }
    }

    /// Touchpad pinch — begin/update/end; update carries deltas
    /// plus the accumulated scale factor and rotation angle.
    pub fn pinch(
        &mut self,
        phase: Phase,
        dx: f64,
        dy: f64,
        scale: f64,
        rotation: f64,
        fingers: u32,
        cancelled: bool,
    ) {
        let Some(focus) = self.focus.clone() else {
            return;
        };
        let (serial, time) = (self.next_serial_pub(), now_ms());
        self.pinch_gestures.retain(|o| o.is_alive());
        for obj in &self.pinch_gestures {
            let ok = obj
                .data::<wl_pointer::WlPointer>()
                .is_some_and(|p| owns_focus(self, p, &focus));
            if !ok {
                continue;
            }
            match phase {
                Phase::Begin => obj.begin(serial, time, &focus, fingers),
                Phase::Update => obj.update(time, dx, dy, scale, rotation),
                Phase::End => obj.end(serial, time, cancelled as i32),
            }
        }
    }

    /// Touchpad hold — begin/end only, no update events.
    pub fn hold(&mut self, phase: Phase, fingers: u32, cancelled: bool) {
        let Some(focus) = self.focus.clone() else {
            return;
        };
        let (serial, time) = (self.next_serial_pub(), now_ms());
        self.hold_gestures.retain(|o| o.is_alive());
        for obj in &self.hold_gestures {
            let ok = obj
                .data::<wl_pointer::WlPointer>()
                .is_some_and(|p| owns_focus(self, p, &focus));
            if !ok {
                continue;
            }
            match phase {
                Phase::Begin => obj.begin(serial, time, &focus, fingers),
                Phase::End => obj.end(serial, time, cancelled as i32),
                // libinput produces no hold-update; ignore.
                _ => {}
            }
        }
    }
}
