// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Virtual-pointer injector — drives real pointer motion and
//! clicks through zwlr_virtual_pointer_v1 into whatever
//! compositor WAYLAND_DISPLAY points at. Exists so the X11
//! input path can be verified end-to-end: inject over an xev
//! window and read its event list.
//!
//! Usage: VPTR_X=400 VPTR_Y=300 ./vptr_inject
//! Moves to (x,y) in 20 steps, clicks left, then moves away.

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1 as zvp,
};

#[derive(Default)]
struct App {
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<ZwlrVirtualPointerManagerV1>,
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
                "zwlr_virtual_pointer_manager_v1" => app.mgr = Some(reg.bind(name, 2, qh, ())),
                _ => {}
            }
        }
    }
}

macro_rules! noop {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
noop!(
    wl_seat::WlSeat,
    ZwlrVirtualPointerManagerV1,
    zvp::ZwlrVirtualPointerV1
);

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _ = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    eq.roundtrip(&mut app).unwrap();

    let Some(mgr) = app.mgr.clone() else {
        eprintln!("no zwlr_virtual_pointer_manager_v1 — aborting");
        std::process::exit(1);
    };
    // The seat argument picks which seat the virtual device
    // feeds; None lets the compositor choose its only seat.
    let vptr = mgr.create_virtual_pointer(app.seat.as_ref(), &qh, ());
    let tx: f64 = std::env::var("VPTR_X")
        .unwrap_or("400".into())
        .parse()
        .unwrap();
    let ty: f64 = std::env::var("VPTR_Y")
        .unwrap_or("300".into())
        .parse()
        .unwrap();
    // Extents describe the coordinate space the absolute
    // position is scaled from — a generous desktop rect keeps
    // the mapping 1:1 on every compositor.
    let (ex, ey): (u32, u32) = (8192, 4096);
    let mut ms: u32 = 0;
    for i in 1..=20 {
        ms += 8;
        vptr.motion_absolute(
            ms,
            (tx * i as f64 / 20.0) as u32,
            (ty * i as f64 / 20.0) as u32,
            ex,
            ey,
        );
        vptr.frame();
    }
    // Let the motion batch land before the click — compositors
    // batch by frame.
    eq.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(120));
    ms += 8;
    vptr.button(
        ms,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Pressed,
    );
    vptr.frame();
    ms += 8;
    vptr.button(
        ms,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Released,
    );
    vptr.frame();
    eq.flush().unwrap();
    println!("injected motion → ({tx},{ty}) + click");
}
