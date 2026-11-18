// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_keyboard_shortcuts_inhibit_v1 test — map a toplevel
//! (last-mapped takes keyboard focus in Odin), claim shortcut
//! inhibition and verify:
//!
//!   1. the inhibitor receives `active` once focus lands
//!   2. a key combo that IS a compositor binding (C-A-d =
//!      dump-frame) still reaches THIS client's wl_keyboard —
//!      proof the binding was suspended for us
//!
//! The selftest injects the combo a few ticks after spawning
//! this client. Prints RESULT inhibited=…

use std::io::Write;
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1 as ksi,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    active: bool,
    // evdev codes of keys that reached us while inhibited —
    // the selftest sends C-A-d (29+56+32).
    keys: Vec<u32>,
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
                "wl_compositor" => app.comp = Some(reg.bind(name, 5, qh, ())),
                "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(reg.bind(name, 3, qh, ())),
                "wl_seat" => app.seat = Some(reg.bind(name, 8, qh, ())),
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    app.mgr = Some(reg.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ksi::ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ksi::ZwpKeyboardShortcutsInhibitorV1,
        event: ksi::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ksi::Event::Active => {
                println!("INHIBIT-ACTIVE");
                app.active = true;
            }
            ksi::Event::Inactive => {
                println!("INHIBIT-INACTIVE");
                app.active = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            println!("INHIBIT-KEY {key}");
            app.keys.push(key);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        xs: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xs.ack_configure(serial);
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
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wl_seat::WlSeat,
    xdg_wm_base::XdgWmBase,
    xdg_toplevel::XdgToplevel,
    ZwpKeyboardShortcutsInhibitManagerV1
);

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _ = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    eq.roundtrip(&mut app).unwrap();

    if app.mgr.is_none() {
        println!("RESULT inhibited=false missing zwp_keyboard_shortcuts_inhibit_manager_v1");
        std::process::exit(0);
    }

    // Map a real toplevel — inhibiting an unmapped surface gets
    // no focus and therefore never goes active.
    let surf = app.comp.clone().unwrap().create_surface(&qh, ());
    let xs = app.wm.clone().unwrap().get_xdg_surface(&surf, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    tl.set_title("inhibit-test".into());
    let seat = app.seat.clone().unwrap();
    let _kb = seat.get_keyboard(&qh, ());
    let _inh = app
        .mgr
        .clone()
        .unwrap()
        .inhibit_shortcuts(&surf, &seat, &qh, ());
    // 64x64 solid buffer — enough for the compositor to map us.
    let path = std::env::temp_dir().join(format!("odin-inhtest-{}", std::process::id()));
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(64 * 64 * 4).unwrap();
        let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(f.as_raw_fd()) };
        let pool = app
            .shm
            .clone()
            .unwrap()
            .create_pool(bfd, 64 * 64 * 4, &qh, ());
        let buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
        surf.attach(Some(&buf), 0, 0);
        let mut wf = std::fs::File::options().write(true).open(&path).unwrap();
        wf.write_all(&vec![80u8, 40, 160, 255].repeat(64 * 64))
            .unwrap();
        surf.commit();
    }
    std::fs::remove_file(&path).ok();
    conn.flush().unwrap();

    // The selftest sends C-A-d shortly after we map — wait for
    // the d key (32) or a generous deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !app.keys.contains(&32) && std::time::Instant::now() < deadline {
        if eq.blocking_dispatch(&mut app).is_err() {
            break;
        }
    }
    let got_d = app.keys.contains(&32);
    // With inhibition active the WHOLE combo must reach us —
    // ctrl(29) and alt(56) included; a suspended binding only
    // swallows nothing.
    let got_mods = app.keys.contains(&29) && app.keys.contains(&56);
    let ok = app.active && got_d && got_mods;
    println!(
        "RESULT inhibited={ok} active={} d={got_d} mods={got_mods}",
        app.active
    );
}
