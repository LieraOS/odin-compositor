// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! libinput tablet events → `zwp_tablet_*_v2` event streams.
//! Tool objects announce on first proximity; events then reach
//! only the seat whose client owns the surface under the tool —
//! the same focus discipline the pointer path enforces.

use input::event::tablet_pad::{
    RingAxisSource, StripAxisSource, TabletPadEvent, TabletPadEventTrait, TabletPadRingEvent,
    TabletPadStripEvent,
};
use input::event::tablet_tool::{
    ProximityState, TabletToolAxisEvent, TabletToolButtonEvent, TabletToolEvent,
    TabletToolEventTrait, TabletToolProximityEvent, TabletToolTipEvent, TipState,
};
use input::event::EventTrait;
use std::sync::Mutex;
use wayland_protocols::wp::tablet::zv2::server::{
    zwp_tablet_pad_ring_v2 as pr, zwp_tablet_pad_strip_v2 as pst, zwp_tablet_pad_v2 as pdrv,
    zwp_tablet_tool_v2 as ttv, zwp_tablet_tool_v2::ZwpTabletToolV2,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::tablet::{TabletDev, ToolState, ZwpTablet, ZwpTabletSeat};
use super::CompState;
impl Dispatch<ZwpTabletToolV2, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTabletToolV2,
        request: ttv::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // A stylus cursor shares the pointer's cursor slot —
            // hotspot semantics identical to wl_pointer.set_cursor.
            ttv::Request::SetCursor {
                surface,
                hotspot_x,
                hotspot_y,
                ..
            } => {
                state.cursor_surf = surface;
                state.cursor_hotspot = (hotspot_x, hotspot_y);
                state.cursor_gen += 1;
            }
            ttv::Request::Destroy => {}
            _ => {}
        }
    }
}

impl CompState {
    /// One libinput tablet-tool event — dispatch by variant.
    pub(crate) fn tablet_tool(&mut self, ev: &TabletToolEvent) {
        match ev {
            TabletToolEvent::Proximity(e) => self.tool_proximity(e),
            TabletToolEvent::Axis(e) => self.tool_axis(e),
            TabletToolEvent::Tip(e) => self.tool_tip(e),
            TabletToolEvent::Button(e) => self.tool_button(e),
            _ => {}
        }
    }

    /// Pad panels — buttons, rings, strips.
    pub(crate) fn tablet_pad(&mut self, ev: &TabletPadEvent) {
        match ev {
            TabletPadEvent::Button(e) => {
                let serial = self.next_serial_pub();
                let sys = e.device().sysname().to_string();
                let pressed = matches!(
                    e.button_state(),
                    input::event::pointer::ButtonState::Pressed
                );
                if let Some(dev) = self.pads.iter().find(|p| p.sysname == sys) {
                    for (_, o) in &dev.objs {
                        o.pad.button(
                            serial,
                            e.button_number(),
                            if pressed {
                                pdrv::ButtonState::Pressed.into()
                            } else {
                                pdrv::ButtonState::Released.into()
                            },
                        );
                    }
                }
            }
            TabletPadEvent::Ring(e) => self.pad_ring(e),
            TabletPadEvent::Strip(e) => self.pad_strip(e),
            _ => {}
        }
    }

    /// Ring turn → source + angle + frame on the indexed ring
    /// object; a position of -1 means the finger lifted (stop).
    fn pad_ring(&mut self, e: &TabletPadRingEvent) {
        let sys = e.device().sysname().to_string();
        let Some(dev) = self.pads.iter().find(|p| p.sysname == sys) else {
            return;
        };
        let pos = e.position();
        for (_, o) in &dev.objs {
            for g in &o.groups {
                if let Some(r) = g.rings.get(e.number() as usize) {
                    // The source enum has finger only — other
                    // sources just skip the source event.
                    if matches!(e.source(), RingAxisSource::Finger) {
                        r.source(pr::Source::Finger.into());
                    }
                    if pos >= 0.0 {
                        r.angle(pos);
                    } else {
                        r.stop();
                    }
                    r.frame(e.time());
                }
            }
        }
    }

    /// Strip slide → source + position + frame, same index rule.
    fn pad_strip(&mut self, e: &TabletPadStripEvent) {
        let sys = e.device().sysname().to_string();
        let Some(dev) = self.pads.iter().find(|p| p.sysname == sys) else {
            return;
        };
        let pos = e.position();
        for (_, o) in &dev.objs {
            for g in &o.groups {
                if let Some(s) = g.strips.get(e.number() as usize) {
                    if matches!(e.source(), StripAxisSource::Finger) {
                        s.source(pst::Source::Finger.into());
                    }
                    if pos >= 0.0 {
                        // libinput normalizes [0,1]; the protocol
                        // carries uint 0..=65535.
                        s.position((pos * 65535.0).clamp(0.0, 65535.0) as u32);
                    } else {
                        s.stop();
                    }
                    s.frame(e.time());
                }
            }
        }
    }

    /// Proximity in: announce the tool if new, focus the surface
    /// under the tip, emit proximity_in. Out ends the stream.
    fn tool_proximity(&mut self, e: &TabletToolProximityEvent) {
        let tool = e.tool();
        match e.proximity_state() {
            ProximityState::In => {
                let x = e.x_transformed(self.space.0 as u32);
                let y = e.y_transformed(self.space.1 as u32);
                let Some((surf, _lx, _ly)) = self.hit(x, y) else {
                    return;
                };
                let i = self.ensure_tool(&tool);
                *self.tools[i].focused.lock().unwrap() = Some(surf.clone());
                let serial = self.next_serial_pub();
                for (seat, obj) in &self.tools[i].objs {
                    if self.client_matches(&surf, seat.client()) {
                        if let Some(tab) = self.tablet_for_seat(seat) {
                            obj.proximity_in(serial, &tab, &surf);
                        }
                    }
                }
            }
            ProximityState::Out => {
                if let Some(i) = self.tool_index(tool.serial()) {
                    *self.tools[i].focused.lock().unwrap() = None;
                    for (_, obj) in &self.tools[i].objs {
                        obj.proximity_out();
                    }
                }
            }
        }
    }

    /// Axis events → motion + every changed axis + frame.
    fn tool_axis(&mut self, e: &TabletToolAxisEvent) {
        let Some(i) = self.tool_index(e.tool().serial()) else {
            return;
        };
        let Some(surf) = self.tools[i].focused.lock().unwrap().clone() else {
            return;
        };
        let x = e.x_transformed(self.space.0 as u32);
        let y = e.y_transformed(self.space.1 as u32);
        // Surface-local coords — the hit also re-validates that
        // the tool is still over its focus surface.
        let Some((_, lx, ly)) = self.hit(x, y).filter(|(s, _, _)| s.id() == surf.id()) else {
            return;
        };
        for (seat, obj) in &self.tools[i].objs {
            if !self.client_matches(&surf, seat.client()) {
                continue;
            }
            obj.motion(lx, ly);
            if e.pressure_has_changed() {
                // libinput normalizes [0,1]; the protocol carries 0..=65535.
                obj.pressure((e.pressure() * 65535.0).clamp(0.0, 65535.0) as u32);
            }
            if e.distance_has_changed() {
                obj.distance((e.distance() * 65535.0).clamp(0.0, 65535.0) as u32);
            }
            if e.tilt_x_has_changed() || e.tilt_y_has_changed() {
                obj.tilt(e.tilt_x(), e.tilt_y());
            }
            if e.rotation_has_changed() {
                obj.rotation(e.rotation());
            }
            if e.slider_has_changed() {
                // Signed normalization: libinput [-1,1] → ±65535.
                obj.slider((e.slider_position() * 65535.0).clamp(-65535.0, 65535.0) as i32);
            }
            if e.wheel_has_changed() {
                obj.wheel(e.wheel_delta(), e.wheel_delta_discrete() as i32);
            }
            obj.frame(e.time());
        }
    }

    /// Tip down/up — the serial is the focus-grab anchor clients
    /// use for drag semantics, same role as the pointer's.
    fn tool_tip(&mut self, e: &TabletToolTipEvent) {
        let Some(i) = self.tool_index(e.tool().serial()) else {
            return;
        };
        let Some(surf) = self.tools[i].focused.lock().unwrap().clone() else {
            return;
        };
        let serial = self.next_serial_pub();
        let down = matches!(e.tip_state(), TipState::Down);
        for (seat, obj) in &self.tools[i].objs {
            if self.client_matches(&surf, seat.client()) {
                if down {
                    obj.down(serial);
                } else {
                    obj.up();
                }
                obj.frame(e.time());
            }
        }
    }

    /// Stylus barrel buttons — button + serial + state + frame.
    fn tool_button(&mut self, e: &TabletToolButtonEvent) {
        let Some(i) = self.tool_index(e.tool().serial()) else {
            return;
        };
        let Some(surf) = self.tools[i].focused.lock().unwrap().clone() else {
            return;
        };
        let serial = self.next_serial_pub();
        let pressed = matches!(
            e.button_state(),
            input::event::pointer::ButtonState::Pressed
        );
        for (seat, obj) in &self.tools[i].objs {
            if self.client_matches(&surf, seat.client()) {
                obj.button(
                    serial,
                    e.button(),
                    if pressed {
                        ttv::ButtonState::Pressed.into()
                    } else {
                        ttv::ButtonState::Released.into()
                    },
                );
                obj.frame(e.time());
            }
        }
    }

    /// Tool index by libinput serial — the stable identity.
    fn tool_index(&self, serial: u64) -> Option<usize> {
        self.tools.iter().position(|t| t.serial == serial)
    }

    /// Get-or-create a ToolState: on first sight every bound seat
    /// gets a tool object and the type/capability announce block.
    fn ensure_tool(&mut self, tool: &input::event::tablet_tool::TabletTool) -> usize {
        if let Some(i) = self.tool_index(tool.serial()) {
            return i;
        }
        let mut objs = Vec::with_capacity(self.tablet_seats.len());
        for seat in self.tablet_seats.clone() {
            let Some(client) = seat.client() else {
                continue;
            };
            let Ok(obj) = client.create_resource::<ZwpTabletToolV2, (), CompState>(
                &self.dh,
                seat.version(),
                (),
            ) else {
                continue;
            };
            // Type + capability announce before tool_added fires.
            if let Some(ty) = tool.tool_type() {
                obj._type(map_tool_type(ty));
            }
            obj.hardware_serial((tool.serial() >> 32) as u32, tool.serial() as u32);
            for (has, cap) in [
                (tool.has_pressure(), ttv::Capability::Pressure),
                (tool.has_distance(), ttv::Capability::Distance),
                (tool.has_tilt(), ttv::Capability::Tilt),
                (tool.has_rotation(), ttv::Capability::Rotation),
                (tool.has_slider(), ttv::Capability::Slider),
                (tool.has_wheel(), ttv::Capability::Wheel),
            ] {
                if has {
                    obj.capability(cap);
                }
            }
            obj.done();
            seat.tool_added(&obj);
            objs.push((seat, obj));
        }
        self.tools.push(ToolState {
            serial: tool.serial(),
            objs,
            focused: Mutex::new(None),
        });
        self.tools.len() - 1
    }

    /// The seat's `zwp_tablet_v2` matching the tool's tablet —
    /// proximity_in carries it. First object of that seat wins
    /// (libinput already scoped the event to its device).
    fn tablet_for_seat(&self, seat: &ZwpTabletSeat) -> Option<ZwpTablet> {
        self.tablets
            .iter()
            .flat_map(|t: &TabletDev| t.objs.iter())
            .find(|(s, _)| s.id() == seat.id())
            .map(|(_, t)| t.clone())
    }
}

/// libinput tool types → the protocol's enum — one-to-one.
fn map_tool_type(t: input::event::tablet_tool::TabletToolType) -> ttv::Type {
    use input::event::tablet_tool::TabletToolType as L;
    match t {
        L::Pen => ttv::Type::Pen,
        L::Eraser => ttv::Type::Eraser,
        L::Brush => ttv::Type::Brush,
        L::Pencil => ttv::Type::Pencil,
        L::Airbrush => ttv::Type::Airbrush,
        L::Mouse => ttv::Type::Mouse,
        L::Lens => ttv::Type::Lens,
        _ => ttv::Type::Pen,
    }
}
