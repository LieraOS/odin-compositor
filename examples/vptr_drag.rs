// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Virtual-pointer drag driver — the freeze reproduction tool.
//! Moves to a grab point, holds the left button and orbits the
//! pointer for a fixed duration, then releases. A client that
//! keeps repainting (glxgears FPS, storm commits/s, video
//! frames) must continue while the orbit runs — the historical
//! defect froze every other surface until the motion stopped.
//!
//!   VPTR_X=660 VPTR_Y=205 VPTR_DUR=8 VPTR_STEP=8 ./vptr_drag
//!
//! VPTR_DUR seconds of wobble at ~1/VPTR_STEP kHz motion.

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

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

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
    let vptr = mgr.create_virtual_pointer(app.seat.as_ref(), &qh, ());

    let gx = env_f64("VPTR_X", 660.0) as i32;
    let gy = env_f64("VPTR_Y", 205.0) as i32;
    let dur = env_f64("VPTR_DUR", 8.0);
    let step = env_f64("VPTR_STEP", 8.0).max(1.0);
    // Odin maps absolute motion as pos * space / extent — the
    // extents must equal the compositor space for 1:1 aiming.
    let (ex, ey): (u32, u32) = (
        env_f64("VPTR_EX", 1280.0) as u32,
        env_f64("VPTR_EY", 800.0) as u32,
    );
    let mut ms: u32 = 0;
    let feed = |vptr: &zvp::ZwlrVirtualPointerV1, ms: &mut u32, x: i32, y: i32| {
        *ms += step as u32;
        vptr.motion_absolute(*ms, x.max(0) as u32, y.max(0) as u32, ex, ey);
        vptr.frame();
    };

    // Glide to the grab point, then press.
    for i in 1..=20 {
        feed(&vptr, &mut ms, gx * i / 20, gy * i / 20);
        eq.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(step as u64));
    }
    ms += step as u32;
    vptr.button(
        ms,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Pressed,
    );
    vptr.frame();
    eq.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    eprintln!("drag: pressed at ({gx},{gy}) — orbiting for {dur}s");

    // Rectangular wobble orbit around the grab point — the same
    // "wiggle the grabbed window" motion that froze everything.
    let orbit = [(0, -110), (110, -110), (110, 0), (0, 0)];
    let steps = ((dur * 1000.0) / step) as usize;
    for i in 0..steps {
        let (cx, cy) = orbit[i % orbit.len()];
        let (nx, ny) = orbit[(i + 1) % orbit.len()];
        // Interpolate between orbit corners for continuous motion.
        let t = (i % 4) as f64 / 4.0;
        let x = gx + cx + ((nx - cx) as f64 * t) as i32;
        let y = gy + cy + ((ny - cy) as f64 * t) as i32;
        feed(&vptr, &mut ms, x, y);
        eq.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(step as u64));
    }

    ms += step as u32;
    vptr.button(
        ms,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Released,
    );
    vptr.frame();
    eq.flush().unwrap();
    println!("drag: released after {dur}s at ({gx},{gy})");
}
