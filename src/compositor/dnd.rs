// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Drag & drop routing — wl_data_device DnD.
//!
//! A drag is an implicit grab: while it runs, pointer events
//! stay silent and only drag_enter/motion/leave/drop flow —
//! to the data device of whichever client owns the surface
//! under the cursor. The destination negotiates mime + action
//! on its offer; the source learns both through target/action
//! events. receive() after drop pipes data exactly like the
//! selection path does.

use tracing::info;
use wayland_server::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_surface,
};
use wayland_server::Resource;

use super::data::{OfferData, SelSrc, SourceData};
use super::{now_ms, CompState};

/// One in-flight drag — created by data_device.start_drag,
/// lives until drop + finish or cancel.
pub struct Drag {
    /// The offering client's source object.
    pub source: wl_data_source::WlDataSource,
    /// Surface the drag started on.
    /// Optional drag-icon surface — follows the cursor.
    pub icon: Option<wl_surface::WlSurface>,
    /// Surface currently entered as drop target.
    pub focus: Option<wl_surface::WlSurface>,
    /// The live offer handed to `focus`'s device.
    pub offer: Option<wl_data_offer::WlDataOffer>,
    /// Mime the destination accepted via offer.accept.
    pub accepted: Option<String>,
}

impl CompState {
    /// data_device.start_drag — record the grab. Only honored
    /// while a pointer button is held (a drag out of nowhere is
    /// a protocol lie); the serial check would need seat-serial
    /// plumbing tracked per client, which we don't have yet.
    pub fn drag_start(
        &mut self,
        source: Option<wl_data_source::WlDataSource>,
        icon: Option<wl_surface::WlSurface>,
    ) {
        let Some(source) = source else { return };
        if self.ptr_down == 0 || self.drag.is_some() {
            return;
        }
        info!("dnd: drag started");
        self.drag = Some(Drag {
            source,
            icon,
            focus: None,
            offer: None,
            accepted: None,
        });
    }

    /// Pointer moved during a drag — retarget the drag focus:
    /// leave the old destination, enter the new one with a
    /// fresh offer carrying the source's mime list. Same
    /// surface → just a motion update in local coords.
    pub fn drag_motion(&mut self, x: f64, y: f64) {
        let Some((surf, lx, ly)) = self.hit(x, y) else {
            self.drag_leave();
            return;
        };
        let same = self
            .drag
            .as_ref()
            .and_then(|d| d.focus.clone())
            .map(|f| f.id() == surf.id())
            .unwrap_or(false);
        if same {
            if let (Some(dev), Some(d)) = (self.device_for(&surf), self.drag.as_ref()) {
                if d.offer.is_some() {
                    dev.motion(now_ms(), lx, ly);
                }
            }
            return;
        }
        self.drag_leave();
        self.drag_enter(&surf, lx, ly);
    }

    /// Build the destination's offer and send enter — the
    /// source's mime list rides on offer events.
    fn drag_enter(&mut self, surf: &wl_surface::WlSurface, lx: f64, ly: f64) {
        let Some(dev) = self.device_for(surf) else {
            return;
        };
        let Some(client) = surf.client() else { return };
        let handle = self.dh.clone();
        let serial = self.next_serial_pub();
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let mimes = drag
            .source
            .data::<SourceData>()
            .map(|d| d.mimes.lock().unwrap().clone())
            .unwrap_or_default();
        let Ok(offer) = client.create_resource::<wl_data_offer::WlDataOffer, OfferData, CompState>(
            &handle,
            dev.version(),
            OfferData {
                source: SelSrc::Wl(drag.source.clone()),
                dnd: true,
            },
        ) else {
            return;
        };
        dev.data_offer(&offer);
        for m in mimes {
            offer.offer(m);
        }
        // The actions this compositor can serve — the
        // destination picks its preference via set_actions.
        offer.source_actions(
            wl_data_device_manager::DndAction::Copy | wl_data_device_manager::DndAction::Move,
        );
        dev.enter(serial, surf, lx, ly, Some(&offer));
        drag.focus = Some(surf.clone());
        drag.offer = Some(offer);
    }

    /// The drag left a destination — retract via drag_leave.
    fn drag_leave(&mut self) {
        if let Some(d) = self.drag.as_mut() {
            if let Some(surf) = d.focus.take() {
                d.offer = None;
                if let Some(dev) = self.device_for(&surf) {
                    dev.leave();
                }
            }
        }
    }

    /// Button released during a drag — drop on the entered
    /// destination, or cancel when nothing took it. The v3
    /// finish lands later via offer.finish.
    pub fn drag_drop(&mut self) {
        let Some(d) = self.drag.as_ref() else { return };
        let src = d.source.clone();
        if let (Some(surf), Some(_)) = (d.focus.clone(), d.offer.clone()) {
            if let Some(dev) = self.device_for(&surf) {
                dev.drop();
            }
            if src.is_alive() {
                src.dnd_drop_performed();
            }
            info!("dnd: dropped");
        } else {
            // Nowhere to land — the source learns the drag was
            // cancelled, not finished.
            if src.is_alive() {
                src.cancelled();
            }
            self.drag = None;
            info!("dnd: cancelled — no target");
        }
    }

    /// offer.finish (v3) — the destination is done receiving;
    /// only now may the source free its side of the drag.
    pub fn drag_finish(&mut self) {
        if let Some(d) = self.drag.take() {
            if d.source.is_alive() {
                d.source.dnd_finished();
            }
            info!("dnd: finished");
        }
    }

    /// offer.accept during motion — tell the source which mime
    /// the destination would take right now.
    pub fn drag_accept(&mut self, mime: Option<String>) {
        if let Some(d) = self.drag.as_mut() {
            d.accepted = mime.clone();
            if d.source.is_alive() {
                d.source.target(mime);
            }
        }
    }

    /// offer.set_actions — relay the destination's preferred
    /// action back to the source and confirm it on the offer.
    pub fn drag_actions(&mut self, actions: u32) {
        if let Some(d) = self.drag.as_ref() {
            // Intersect the destination's wishes with what we
            // can serve — empty means nothing acceptable.
            use wayland_server::protocol::wl_data_device_manager::DndAction;
            let allowed = DndAction::Copy | DndAction::Move;
            let chosen = DndAction::from_bits(actions).unwrap_or(DndAction::empty()) & allowed;
            if let Some(offer) = &d.offer {
                if offer.is_alive() {
                    offer.action(chosen);
                }
            }
            if d.source.is_alive() {
                d.source.action(chosen);
            }
        }
    }

    /// Receive on a drag offer — same pipe model as selection:
    /// the destination's fd goes straight to source.send.
    pub fn drag_receive(&mut self, mime: String, fd: std::os::fd::BorrowedFd<'_>) {
        if let Some(d) = self.drag.as_ref() {
            if d.source.is_alive() {
                d.source.send(mime, fd);
            }
        }
    }

    /// A data_device object for `surf`'s client — drag events
    /// travel through the same devices selections do.
    fn device_for(&self, surf: &wl_surface::WlSurface) -> Option<wl_data_device::WlDataDevice> {
        let client = surf.client()?;
        self.data_devices
            .iter()
            .find(|d| d.is_alive() && d.client().map(|c| c.id()) == Some(client.id()))
            .cloned()
    }
}
