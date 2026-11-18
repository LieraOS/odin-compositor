// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! libinput event source — the raw input pipeline entry point.
//!
//! Phase 1 scope: claim seat0, drain every event and log it. The
//! pointer/keyboard dispatch into the focus engine lands in
//! phase 2 — this proves the session-owned device path.

use anyhow::{Context, Result};
use input::event::gesture::{
    GestureEndEvent, GestureEventCoordinates, GestureEventTrait, GesturePinchEventTrait,
};
use input::event::keyboard::KeyboardEventTrait;
use input::event::pointer::{Axis, PointerEventTrait as _, PointerScrollEvent};
use input::event::touch::{TouchEventPosition, TouchEventSlot};
use input::event::EventTrait as _;
use input::event::{Event, GestureEvent, KeyboardEvent, PointerEvent, TouchEvent};
use input::{Libinput, LibinputInterface};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use tracing::info;

/// libinput's device-open callback. For this milestone devices are
/// opened directly — the seat-managed path (`Session::open_device`)
/// replaces this when the input loop moves into the event loop.
struct DirectOpen;
impl LibinputInterface for DirectOpen {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        // libinput demands O_NONBLOCK on the fds it gets — without it
        // the first dispatch() blocks forever in evdev_read when no
        // event is pending (observed: compositor hung in init).
        // O_CLOEXEC and friends ride along in `flags` too — pass
        // them through instead of reconstructing.
        std::fs::OpenOptions::new()
            .read(true)
            .write(flags & libc::O_ACCMODE != libc::O_RDONLY)
            .custom_flags(flags & !libc::O_ACCMODE)
            .open(path)
            .map(Into::into)
            .map_err(|err| err.raw_os_error().unwrap_or(libc::EIO))
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd)
    }
}

/// Initialise libinput on the session seat — the returned handle
/// is registered as an fd source on the calloop loop.
pub fn init(_session: &mut crate::session::Session) -> Result<Libinput> {
    let mut li = Libinput::new_with_udev(DirectOpen);
    li.udev_assign_seat("seat0")
        .map_err(|_| anyhow::anyhow!("udev_assign_seat failed — libseat backend running?"))?;
    li.dispatch().context("initial dispatch")?;
    info!("libinput claimed seat0");
    Ok(li)
}

/// Drain one batch of pending events — the fd-source callback.
/// Events route into the compositor's input model (pointer pos,
/// buttons, keys+bindings) — the udev equivalent of the host
/// forwarding the nested backend does.
pub fn drain(li: &mut Libinput, comp: &mut crate::compositor::CompState) -> Result<()> {
    li.dispatch().context("libinput dispatch")?;
    // Event timestamps are CLOCK_MONOTONIC microseconds since
    // boot — read the same clock once per batch so the gap
    // between event time and drain time is the true loop-
    // starvation latency (what the user feels as a slow pointer).
    let now_us = {
        let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        (ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000) as u64
    };
    let mut oldest_ms = 0.0f64;
    for event in li {
        let usec = match &event {
            Event::Pointer(e) => match e {
                PointerEvent::Motion(ev) => Some(ev.time_usec()),
                PointerEvent::MotionAbsolute(ev) => Some(ev.time_usec()),
                PointerEvent::Button(ev) => Some(ev.time_usec()),
                PointerEvent::ScrollWheel(ev) => Some(ev.time_usec()),
                PointerEvent::ScrollFinger(ev) => Some(ev.time_usec()),
                PointerEvent::ScrollContinuous(ev) => Some(ev.time_usec()),
                _ => None,
            },
            Event::Keyboard(KeyboardEvent::Key(ev)) => Some(ev.time_usec()),
            _ => None,
        };
        if let Some(us) = usec {
            let lag_us = now_us.saturating_sub(us) as f64;
            oldest_ms = oldest_ms.max(lag_us / 1_000.0);
        }
        match event {
            Event::Pointer(PointerEvent::Motion(e)) => {
                // dx/dy carry libinput's pointer acceleration —
                // what the user feels. The unaccelerated pair is
                // the raw hardware delta for relative-pointer
                // clients (games), which apply their own curve.
                comp.ptr_rel(e.dx(), e.dy(), e.dx_unaccelerated(), e.dy_unaccelerated());
            }
            Event::Pointer(PointerEvent::MotionAbsolute(e)) => {
                comp.ptr_motion(e.absolute_x(), e.absolute_y());
            }
            Event::Pointer(PointerEvent::Button(e)) => {
                comp.ptr_button(
                    e.button(),
                    e.button_state() == input::event::pointer::ButtonState::Pressed,
                );
            }
            Event::Keyboard(KeyboardEvent::Key(e)) => {
                comp.key_event(
                    e.key(),
                    e.key_state() == input::event::keyboard::KeyState::Pressed,
                );
            }
            // Scroll: wheel, finger (touchpad 2-finger) and
            // continuous scroll all arrive as axis events — the
            // trait is shared, so one arm covers every source.
            Event::Pointer(PointerEvent::ScrollWheel(e)) => {
                // libinput v120: 120 per wheel click; Wayland
                // convention is 10.0 per step — same positive =
                // down sign convention, so no inversion.
                // source: 0=wheel 1=finger 2=continuous
                // Querying an unset axis makes libinput log a
                // "client bug" per event — check before reading.
                if e.has_axis(Axis::Vertical) {
                    comp.ptr_axis(0, e.scroll_value_v120(Axis::Vertical) / 12.0, 0);
                }
                if e.has_axis(Axis::Horizontal) {
                    comp.ptr_axis(1, e.scroll_value_v120(Axis::Horizontal) / 12.0, 0);
                }
            }
            Event::Pointer(PointerEvent::ScrollFinger(e)) => {
                if e.has_axis(Axis::Vertical) {
                    comp.ptr_axis(0, e.scroll_value(Axis::Vertical), 1);
                }
                if e.has_axis(Axis::Horizontal) {
                    comp.ptr_axis(1, e.scroll_value(Axis::Horizontal), 1);
                }
            }
            Event::Pointer(PointerEvent::ScrollContinuous(e)) => {
                if e.has_axis(Axis::Vertical) {
                    comp.ptr_axis(0, e.scroll_value(Axis::Vertical), 2);
                }
                if e.has_axis(Axis::Horizontal) {
                    comp.ptr_axis(1, e.scroll_value(Axis::Horizontal), 2);
                }
            }
            // Touchpad gestures — libinput translates 3/4-finger
            // swipes and 2-finger pinches into these; the
            // compositor forwards them to gesture-protocol clients
            // whose pointer holds focus.
            Event::Gesture(GestureEvent::Swipe(ev)) => {
                use crate::compositor::gestures::Phase;
                use input::event::gesture::GestureSwipeEvent as G;
                match ev {
                    G::Begin(e) => {
                        comp.swipe(Phase::Begin, 0.0, 0.0, e.finger_count() as u32, false)
                    }
                    G::Update(e) => comp.swipe(
                        Phase::Update,
                        e.dx(),
                        e.dy(),
                        e.finger_count() as u32,
                        false,
                    ),
                    G::End(e) => {
                        comp.swipe(Phase::End, 0.0, 0.0, e.finger_count() as u32, e.cancelled())
                    }
                    _ => {}
                }
            }
            Event::Gesture(GestureEvent::Pinch(ev)) => {
                use crate::compositor::gestures::Phase;
                use input::event::gesture::GesturePinchEvent as G;
                match ev {
                    G::Begin(e) => comp.pinch(
                        Phase::Begin,
                        0.0,
                        0.0,
                        1.0,
                        0.0,
                        e.finger_count() as u32,
                        false,
                    ),
                    G::Update(e) => comp.pinch(
                        Phase::Update,
                        e.dx(),
                        e.dy(),
                        e.scale(),
                        e.angle_delta(),
                        e.finger_count() as u32,
                        false,
                    ),
                    G::End(e) => comp.pinch(
                        Phase::End,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        e.finger_count() as u32,
                        e.cancelled(),
                    ),
                    _ => {}
                }
            }
            Event::Gesture(GestureEvent::Hold(ev)) => {
                use crate::compositor::gestures::Phase;
                use input::event::gesture::GestureHoldEvent as G;
                match ev {
                    G::Begin(e) => comp.hold(Phase::Begin, e.finger_count() as u32, false),
                    G::End(e) => comp.hold(Phase::End, e.finger_count() as u32, e.cancelled()),
                    _ => {}
                }
            }
            // Touchscreen — slot-tracked points grab the surface
            // under the down position; frame ends each batch.
            Event::Touch(TouchEvent::Down(e)) => {
                comp.touch_down(
                    e.seat_slot() as i32,
                    e.x_transformed(comp.space.0 as u32),
                    e.y_transformed(comp.space.1 as u32),
                );
            }
            Event::Touch(TouchEvent::Motion(e)) => {
                comp.touch_motion(
                    e.seat_slot() as i32,
                    e.x_transformed(comp.space.0 as u32),
                    e.y_transformed(comp.space.1 as u32),
                );
            }
            Event::Touch(TouchEvent::Up(e)) => {
                comp.touch_up(e.seat_slot() as i32);
            }
            Event::Touch(TouchEvent::Cancel(_)) => {
                comp.touch_cancel();
            }
            Event::Touch(TouchEvent::Frame(_)) => {
                comp.touch_frame();
            }
            // zwp_tablet_manager_v2 — device hotplug announces
            // tablets/pads; tool and pad events feed the streams.
            Event::Device(input::event::DeviceEvent::Added(e)) => {
                let mut dev = e.device();
                // Per-device settings from input.conf — pointer
                // accel/speed, tap-to-click, natural scroll —
                // applied at add time so they survive libinput's
                // own defaults for every device, hotplug or boot.
                crate::input_conf::apply_device(&mut dev, &comp.cfg.input);
                comp.tablet_device_added(&dev);
            }
            Event::Tablet(e) => {
                comp.tablet_tool(&e);
            }
            Event::TabletPad(e) => {
                comp.tablet_pad(&e);
            }
            _ => {}
        }
    }
    comp.stat_input_ms = comp.stat_input_ms.max(oldest_ms);
    Ok(())
}
