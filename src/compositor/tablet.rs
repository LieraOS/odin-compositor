// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `zwp_tablet_manager_v2` — graphics tablets.
//! Per seat the client binds a `zwp_tablet_seat_v2`; physical
//! tablets announce as `zwp_tablet_v2` (name/id/path), styli as
//! `zwp_tablet_tool_v2` on first proximity, pad panels as
//! `zwp_tablet_pad_v2` with ring/strip/group children. libinput
//! events arrive via `tablet_events.rs`; this file owns object
//! lifetimes and the seat/device bookkeeping.

use input::DeviceCapability;
use std::sync::Mutex;
use wayland_protocols::wp::tablet::zv2::server::{
    zwp_tablet_manager_v2 as tm, zwp_tablet_manager_v2::ZwpTabletManagerV2,
    zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
    zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, zwp_tablet_pad_v2::ZwpTabletPadV2,
    zwp_tablet_seat_v2 as ts, zwp_tablet_seat_v2::ZwpTabletSeatV2,
    zwp_tablet_tool_v2::ZwpTabletToolV2, zwp_tablet_v2::ZwpTabletV2,
};
use wayland_server::protocol::{wl_seat, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols::wp::tablet::zv2::server::zwp_tablet_manager_v2::ZwpTabletManagerV2 as ZwpTabletManager;

pub use wayland_protocols::wp::tablet::zv2::server::zwp_tablet_seat_v2::ZwpTabletSeatV2 as ZwpTabletSeat;

pub use wayland_protocols::wp::tablet::zv2::server::zwp_tablet_v2::ZwpTabletV2 as ZwpTablet;

/// One physical tablet — every bound seat holds its own
/// `zwp_tablet_v2` view of it.
pub(crate) struct TabletDev {
    /// libinput sysname — identity for matching remove events.
    pub sysname: String,
    pub name: String,
    pub vid: u32,
    pub pid: u32,
    pub path: String,
    /// (seat, announced tablet object) pairs.
    pub objs: Vec<(ZwpTabletSeatV2, ZwpTabletV2)>,
}

/// A stylus libinput saw at least once — keyed by its hardware
/// serial so re-proximity reuses the same tool objects.
pub(crate) struct ToolState {
    pub serial: u64,
    /// (seat, tool object) pairs.
    pub objs: Vec<(ZwpTabletSeatV2, ZwpTabletToolV2)>,
    /// Surface under proximity_in — events reach only the seat
    /// client owning it.
    pub focused: Mutex<Option<wl_surface::WlSurface>>,
}

/// One mode group with its ring/strip children — rings and
/// strips belong to groups, not the pad itself (per protocol:
/// `zwp_tablet_pad_v2.group` announces the group object, which
/// then announces its controls).
pub(crate) struct PadGroupObjs {
    /// The group resource — held alive so its ring/strip
    /// children stay valid (mode-switch events hook here when
    /// pad mode support lands).
    #[allow(dead_code)]
    pub group: ZwpTabletPadGroupV2,
    pub rings: Vec<ZwpTabletPadRingV2>,
    pub strips: Vec<ZwpTabletPadStripV2>,
}

/// One seat's view of a pad — pad object plus its mode groups.
pub(crate) struct PadSeatObjs {
    pub pad: ZwpTabletPadV2,
    pub groups: Vec<PadGroupObjs>,
}

/// A pad panel (buttons/rings/strips/mode groups) on a tablet.
pub(crate) struct PadDev {
    pub sysname: String,
    /// (seat, pad view) pairs.
    pub objs: Vec<(ZwpTabletSeatV2, PadSeatObjs)>,
    /// libinput-reported control counts for the announce.
    pub buttons: u32,
    pub rings: usize,
    pub strips: usize,
    pub groups: usize,
}

impl Dispatch<ZwpTabletManagerV2, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTabletManagerV2,
        request: tm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            tm::Request::GetTabletSeat { tablet_seat, seat } => {
                let seat_obj =
                    init.init::<ZwpTabletSeatV2, wl_seat::WlSeat>(tablet_seat, seat.clone());
                state.tablet_seats.push(seat_obj.clone());
                // Late binders learn about already-attached
                // hardware — the same announce the hotplug path
                // sends.
                state.tablet_announce_seat(&seat_obj);
            }
            tm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpTabletSeatV2, wl_seat::WlSeat> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwpTabletSeatV2,
        request: ts::Request,
        _: &wl_seat::WlSeat,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, ts::Request::Destroy) {
            // Drop this seat's view of every device and tool.
            state.tablet_seats.retain(|s| s.id() != obj.id());
            for t in &mut state.tablets {
                t.objs.retain(|(s, _)| s.id() != obj.id());
            }
            for t in &mut state.tools {
                t.objs.retain(|(s, _)| s.id() != obj.id());
            }
            for p in &mut state.pads {
                p.objs.retain(|(s, _)| s.id() != obj.id());
            }
        }
    }
}

/// Device-object dispatches — destroy is their only request.
macro_rules! dev_dispatch {
    ($t:ty, $r:ty) => {
        impl Dispatch<$t, ()> for CompState {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$t,
                _: $r,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}

dev_dispatch!(
    ZwpTabletV2,
    wayland_protocols::wp::tablet::zv2::server::zwp_tablet_v2::Request
);
dev_dispatch!(
    ZwpTabletPadV2,
    wayland_protocols::wp::tablet::zv2::server::zwp_tablet_pad_v2::Request
);
dev_dispatch!(
    ZwpTabletPadRingV2,
    wayland_protocols::wp::tablet::zv2::server::zwp_tablet_pad_ring_v2::Request
);
dev_dispatch!(
    ZwpTabletPadStripV2,
    wayland_protocols::wp::tablet::zv2::server::zwp_tablet_pad_strip_v2::Request
);
dev_dispatch!(
    ZwpTabletPadGroupV2,
    wayland_protocols::wp::tablet::zv2::server::zwp_tablet_pad_group_v2::Request
);

impl CompState {
    /// A libinput device appeared — announce it to every bound
    /// tablet seat according to the capability set libinput
    /// reports for it.
    pub(crate) fn tablet_device_added(&mut self, dev: &input::Device) {
        let tool = dev.has_capability(DeviceCapability::TabletTool);
        let pad = dev.has_capability(DeviceCapability::TabletPad);
        if !tool && !pad {
            return;
        }
        let sysname = dev.sysname().to_string();
        if tool && self.tablets.iter().any(|t| t.sysname == sysname)
            || pad && self.pads.iter().any(|p| p.sysname == sysname)
        {
            return;
        }
        if tool {
            let t = TabletDev {
                sysname: sysname.clone(),
                name: dev.name().to_string(),
                vid: dev.id_vendor(),
                pid: dev.id_product(),
                // udev_device is unsafe in the input crate —
                // the event path is a safe, stable identity.
                path: format!("/dev/input/{}", dev.sysname()),
                objs: Vec::new(),
            };
            self.tablets.push(t);
            let i = self.tablets.len() - 1;
            for seat in self.tablet_seats.clone() {
                if let Some(t) = self.announce_tablet(seat.clone(), i) {
                    seat.tablet_added(&t);
                }
            }
        }
        if pad {
            let p = PadDev {
                sysname: sysname.clone(),
                objs: Vec::new(),
                buttons: dev.tablet_pad_number_of_buttons().max(0) as u32,
                rings: dev.tablet_pad_number_of_rings().max(0) as usize,
                strips: dev.tablet_pad_number_of_strips().max(0) as usize,
                groups: dev.tablet_pad_number_of_mode_groups().max(0) as usize,
            };
            self.pads.push(p);
            let i = self.pads.len() - 1;
            for seat in self.tablet_seats.clone() {
                if let Some(p) = self.announce_pad(seat.clone(), i) {
                    seat.pad_added(&p.pad);
                    self.pads[i].objs.push((seat, p));
                }
            }
        }
    }

    /// Replay the device announce for a seat that bound after
    /// hardware enumeration already ran.
    fn tablet_announce_seat(&mut self, seat: &ZwpTabletSeatV2) {
        for i in 0..self.tablets.len() {
            if let Some(t) = self.announce_tablet(seat.clone(), i) {
                seat.tablet_added(&t);
            }
        }
        for i in 0..self.pads.len() {
            if let Some(p) = self.announce_pad(seat.clone(), i) {
                seat.pad_added(&p.pad);
                self.pads[i].objs.push((seat.clone(), p));
            }
        }
    }

    /// Create + describe one seat's `zwp_tablet_v2` for tablet
    /// `i`, register the pair, and hand the object back for the
    /// `tablet_added` event.
    fn announce_tablet(&mut self, seat: ZwpTabletSeatV2, i: usize) -> Option<ZwpTabletV2> {
        let dev = &self.tablets[i];
        let t: ZwpTabletV2 = seat
            .client()?
            .create_resource::<ZwpTabletV2, (), CompState>(&self.dh, seat.version(), ())
            .ok()?;
        t.name(dev.name.clone());
        t.id(dev.vid, dev.pid);
        t.path(dev.path.clone());
        t.done();
        self.tablets[i].objs.push((seat, t.clone()));
        Some(t)
    }

    /// Create + describe one seat's pad object set for pad `i` —
    /// groups first (each announcing its ring/strip children and
    /// button list), then path/buttons/done on the pad, exactly
    /// as the protocol orders them.
    fn announce_pad(&mut self, seat: ZwpTabletSeatV2, i: usize) -> Option<PadSeatObjs> {
        let (buttons, rings, strips, groups, path) = {
            let d = &self.pads[i];
            (d.buttons, d.rings, d.strips, d.groups, d.sysname.clone())
        };
        let client = seat.client()?;
        let v = seat.version();
        let pad: ZwpTabletPadV2 = client
            .create_resource::<ZwpTabletPadV2, (), CompState>(&self.dh, v, ())
            .ok()?;
        let mut objs = PadSeatObjs {
            pad: pad.clone(),
            groups: Vec::new(),
        };
        // At least one mode group exists per protocol — libinput
        // reports zero on pads without mode support, which still
        // announces a single implicit group.
        for gi in 0..groups.max(1) {
            let Ok(g) =
                client.create_resource::<ZwpTabletPadGroupV2, (), CompState>(&self.dh, v, ())
            else {
                continue;
            };
            pad.group(&g);
            let mut grp = PadGroupObjs {
                group: g.clone(),
                rings: Vec::new(),
                strips: Vec::new(),
            };
            // Rings/strips attach to the first group — the common
            // single-group hardware layout.
            if gi == 0 {
                for _ in 0..rings {
                    if let Ok(r) =
                        client.create_resource::<ZwpTabletPadRingV2, (), CompState>(&self.dh, v, ())
                    {
                        g.ring(&r);
                        grp.rings.push(r);
                    }
                }
                for _ in 0..strips {
                    if let Ok(s) = client.create_resource::<ZwpTabletPadStripV2, (), CompState>(
                        &self.dh,
                        v,
                        (),
                    ) {
                        g.strip(&s);
                        grp.strips.push(s);
                    }
                }
            }
            g.buttons((0..buttons).map(|b| b as u8).collect::<Vec<u8>>());
            g.done();
            objs.groups.push(grp);
        }
        pad.path(path);
        pad.buttons(buttons);
        pad.done();
        Some(objs)
    }
}
