// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_primary_selection_v1 end-to-end — real protocol client.
//!
//! Claims primary via the wp source path (create_source →
//! offer mimes → set_selection), then serves `send` events by
//! writing the payload into the requester's fd. Run wl-paste
//! --primary against it:
//!
//!   ODIN_BACKEND=nested ./odin &
//!   ./prim_proto_test &
//!   wl-paste --primary -l            # mimes appear
//!   wl-paste --primary -t text/plain # payload comes back

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as psm,
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_device_v1 as psd,
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_offer_v1 as pso,
    zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    zwp_primary_selection_source_v1 as pss,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};

const PAYLOAD: &str = "odin-primary-selection-payload";

#[derive(Default)]
struct App {
    mgr: Option<ZwpPrimarySelectionDeviceManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    sends: u32,
    cancelled: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        reg: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" => app.seat = Some(reg.bind(name, 8, qh, ())),
                "zwp_primary_selection_device_manager_v1" => {
                    app.mgr = Some(reg.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<pss::ZwpPrimarySelectionSourceV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpPrimarySelectionSourceV1,
        event: pss::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Paste requester gave us its fd — write the
            // payload and close it immediately, like a real
            // selection owner is required to.
            pss::Event::Send { mime_type, fd } => {
                use std::io::Write;
                println!("PRIM-SEND mime={mime_type}");
                let mut f = std::fs::File::from(fd);
                f.write_all(PAYLOAD.as_bytes()).unwrap();
                app.sends += 1;
            }
            pss::Event::Cancelled => {
                app.cancelled = true;
                println!("PRIM-CANCELLED");
            }
            _ => {}
        }
    }
}

impl Dispatch<psd::ZwpPrimarySelectionDeviceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpPrimarySelectionDeviceV1,
        event: psd::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let psd::Event::Selection { id } = event {
            println!("PRIM-SEL-EVENT offer={}", id.is_some());
        }
    }
}

impl Dispatch<pso::ZwpPrimarySelectionOfferV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpPrimarySelectionOfferV1,
        _: pso::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<psm::ZwpPrimarySelectionDeviceManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpPrimarySelectionDeviceManagerV1,
        _: psm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App::default();
    let _reg = conn.display().get_registry(&qh, ());
    eq.roundtrip(&mut app).unwrap();

    let mgr = app.mgr.clone().unwrap();
    let src = mgr.create_source(&qh, ());
    src.offer("text/plain".into());
    src.offer("UTF8_STRING".into());
    let dev = mgr.get_device(app.seat.as_ref().unwrap(), &qh, ());
    dev.set_selection(Some(&src), 0);
    eq.roundtrip(&mut app).unwrap();
    println!("PRIM-OWNED");

    // Hold the selection: the harness runs wl-paste against us.
    // ~15 s of dispatch covers both -l and -t runs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        eq.blocking_dispatch(&mut app).unwrap();
        if app.sends >= 3 {
            break;
        }
    }
    println!("RESULT sends={} cancelled={}", app.sends, app.cancelled);
}
