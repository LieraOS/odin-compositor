// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Wlr protocol-block test client — exercises gamma control,
//! input inhibit, virtual pointer and virtual keyboard over one
//! connection and prints a line per verified step:
//!
//!   GAMMA-SIZE <n> / GAMMA-OK — ramp size + set_gamma accepted
//!   INHIBIT-OK                — inhibitor create/destroy cycle
//!   VP-OK / VK-OK             — virtual input requests accepted
//!   RESULT ok=true            — everything passed
//!
//! Run inside a nested odin: WAYLAND_DISPLAY=odin-0

use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    gamma_control::v1::client::{
        zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
        zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
    },
    input_inhibitor::v1::client::{
        zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1,
        zwlr_input_inhibitor_v1::ZwlrInputInhibitorV1,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};

/// Build a memfd holding a u16 identity ramp — `size` entries
/// per channel × 3 channels, the payload set_gamma expects.
fn ramp_fd(size: u32) -> OwnedFd {
    let fd = rustix::fs::memfd_create("gamma-ramp", rustix::fs::MemfdFlags::empty())
        .expect("memfd_create");
    let mut f = std::fs::File::from(fd);
    for i in 0..size as usize {
        let v = (i * 65535 / size.max(1) as usize) as u16;
        for _ in 0..3 {
            f.write_all(&v.to_ne_bytes()).unwrap();
        }
    }
    f.into()
}

struct App {
    gamma_mgr: Option<ZwlrGammaControlManagerV1>,
    inhibit_mgr: Option<ZwlrInputInhibitManagerV1>,
    vp_mgr: Option<ZwlrVirtualPointerManagerV1>,
    vk_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    output: Option<wl_output::WlOutput>,
    seat: Option<wl_seat::WlSeat>,
    gamma_size: u32,
    gamma_failed: bool,
    ok: bool,
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
                "zwlr_gamma_control_manager_v1" => {
                    app.gamma_mgr = Some(reg.bind(name, 1, qh, ()));
                }
                "zwlr_input_inhibit_manager_v1" => {
                    app.inhibit_mgr = Some(reg.bind(name, 1, qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    app.vp_mgr = Some(reg.bind(name, 2, qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    app.vk_mgr = Some(reg.bind(name, 1, qh, ()));
                }
                "wl_output" => {
                    if app.output.is_none() {
                        app.output = Some(reg.bind(name, 1, qh, ()));
                    }
                }
                "wl_seat" => {
                    app.seat = Some(reg.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                println!("GAMMA-SIZE {size}");
                app.gamma_size = size;
            }
            zwlr_gamma_control_v1::Event::Failed => {
                println!("GAMMA-FAILED");
                app.gamma_failed = true;
            }
            _ => {}
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
    wl_output::WlOutput,
    wl_seat::WlSeat,
    ZwlrGammaControlManagerV1,
    ZwlrInputInhibitManagerV1,
    ZwlrInputInhibitorV1,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1
);

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _ = conn.display().get_registry(&qh, ());
    let mut app = App {
        gamma_mgr: None,
        inhibit_mgr: None,
        vp_mgr: None,
        vk_mgr: None,
        output: None,
        seat: None,
        gamma_size: 0,
        gamma_failed: false,
        ok: true,
    };
    eq.roundtrip(&mut app).unwrap();

    // Gamma: control for our output → ramp upload must not fail.
    if let (Some(mgr), Some(out)) = (&app.gamma_mgr, &app.output) {
        let ctl: ZwlrGammaControlV1 = mgr.get_gamma_control(out, &qh, ());
        eq.roundtrip(&mut app).unwrap();
        if app.gamma_size > 0 {
            let fd = ramp_fd(app.gamma_size);
            ctl.set_gamma(fd.as_fd());
            eq.roundtrip(&mut app).unwrap();
            println!("GAMMA-{}", if app.gamma_failed { "FAIL" } else { "OK" });
        } else {
            println!("GAMMA-FAIL no size");
            app.ok = false;
        }
    } else {
        println!("GAMMA-FAIL no manager/output");
        app.ok = false;
    }

    // Inhibit: create → destroy → create must all succeed.
    if let Some(mgr) = app.inhibit_mgr.clone() {
        let inh: ZwlrInputInhibitorV1 = mgr.get_inhibitor(&qh, ());
        eq.roundtrip(&mut app).unwrap();
        inh.destroy();
        let _inh2: ZwlrInputInhibitorV1 = mgr.get_inhibitor(&qh, ());
        eq.roundtrip(&mut app).unwrap();
        println!("INHIBIT-OK");
    } else {
        println!("INHIBIT-FAIL no manager");
        app.ok = false;
    }

    // Virtual pointer: absolute move + delta + click + scroll.
    if let Some(mgr) = &app.vp_mgr {
        let vp: ZwlrVirtualPointerV1 = mgr.create_virtual_pointer(None, &qh, ());
        vp.motion_absolute(0, 100, 100, 1280, 800);
        vp.motion(0, 5.0, 5.0);
        vp.button(0, 0x110, wl_pointer::ButtonState::Pressed);
        vp.button(0, 0x110, wl_pointer::ButtonState::Released);
        vp.axis(0, wl_pointer::Axis::VerticalScroll, -15.0);
        vp.frame();
        eq.roundtrip(&mut app).unwrap();
        println!("VP-OK");
    } else {
        println!("VP-FAIL no manager");
        app.ok = false;
    }

    // Virtual keyboard: (no-)keymap upload, then a key stroke.
    if let (Some(mgr), Some(seat)) = (&app.vk_mgr, &app.seat) {
        let vk: ZwpVirtualKeyboardV1 = mgr.create_virtual_keyboard(seat, &qh, ());
        let fd = rustix::fs::memfd_create("vk-keymap", rustix::fs::MemfdFlags::empty())
            .expect("memfd_create");
        vk.keymap(
            u32::from(wl_keyboard::KeymapFormat::NoKeymap),
            fd.as_fd(),
            0,
        );
        vk.key(0, 30, u32::from(wl_keyboard::KeyState::Pressed));
        vk.key(0, 30, u32::from(wl_keyboard::KeyState::Released));
        vk.modifiers(0, 0, 0, 0);
        eq.roundtrip(&mut app).unwrap();
        println!("VK-OK");
    } else {
        println!("VK-FAIL no manager/seat");
        app.ok = false;
    }

    println!("RESULT ok={}", app.ok);
}
