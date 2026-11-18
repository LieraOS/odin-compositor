// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_text_input_v3 + zwp_input_method_v2 test — two Wayland
//! connections in one process:
//!
//!   IM side : binds zwp_input_method_manager_v2, gets the seat's
//!             input method (like an on-screen-keyboard daemon)
//!   App side: maps a toplevel (takes keyboard focus), creates a
//!             text_input, enables a field with surrounding text
//!
//! Verified end-to-end through the compositor:
//!   1. text_input.enter when the surface gains keyboard focus
//!   2. IM activate + surrounding_text + done on field commit
//!   3. IM commit_string flows back to the field's text_input
//!   4. disable → IM deactivate
//!
//! Prints RESULT ti=… im=… at the end.

use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3, zwp_text_input_v3 as ti3,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2, zwp_input_method_v2 as im2,
};

/// Application-side state — window + text field.
#[derive(Default)]
struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    tim: Option<ZwpTextInputManagerV3>,
    ti: Option<ti3::ZwpTextInputV3>,
    configured: bool,
    entered: bool,
    commit_str: Option<String>,
    done_serial: Option<u32>,
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
                "zwp_text_input_manager_v3" => app.tim = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { .. } = event {
            app.configured = true;
        }
    }
}

impl Dispatch<ti3::ZwpTextInputV3, ()> for App {
    fn event(
        app: &mut Self,
        _: &ti3::ZwpTextInputV3,
        event: ti3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ti3::Event::Enter { .. } => {
                println!("TI-ENTER");
                app.entered = true;
            }
            ti3::Event::Leave { .. } => println!("TI-LEAVE"),
            ti3::Event::PreeditString { .. } => {}
            ti3::Event::CommitString { text } => {
                println!("TI-COMMIT {:?}", text);
                app.commit_str = text;
            }
            ti3::Event::DeleteSurroundingText { .. } => {}
            ti3::Event::Done { serial } => {
                println!("TI-DONE {}", serial);
                app.done_serial = Some(serial);
            }
            _ => {}
        }
    }
}

macro_rules! noop_app {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
noop_app!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    xdg_wm_base::XdgWmBase,
    xdg_surface::XdgSurface,
    wl_seat::WlSeat,
    wl_keyboard::WlKeyboard,
    ZwpTextInputManagerV3
);

/// IME-side state — the daemon owning the input method.
#[derive(Default)]
struct Im {
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<ZwpInputMethodManagerV2>,
    im: Option<im2::ZwpInputMethodV2>,
    activated: bool,
    deactivated: bool,
    surrounding: Option<String>,
    done_seen: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Im {
    fn event(
        im: &mut Self,
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
                "wl_seat" => im.seat = Some(reg.bind(name, 8, qh, ())),
                "zwp_input_method_manager_v2" => im.mgr = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<im2::ZwpInputMethodV2, ()> for Im {
    fn event(
        im: &mut Self,
        _: &im2::ZwpInputMethodV2,
        event: im2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            im2::Event::Activate => {
                println!("IM-ACTIVATE");
                im.activated = true;
            }
            im2::Event::Deactivate => {
                println!("IM-DEACTIVATE");
                im.deactivated = true;
            }
            im2::Event::SurroundingText { text, .. } => {
                println!("IM-SURROUNDING {:?}", text);
                im.surrounding = Some(text);
            }
            im2::Event::Done => {
                println!("IM-DONE");
                im.done_seen = true;
            }
            _ => {}
        }
    }
}

macro_rules! noop_im {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for Im {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
noop_im!(
    wl_seat::WlSeat,
    wl_keyboard::WlKeyboard,
    ZwpInputMethodManagerV2
);

fn main() {
    // Two independent connections — IM daemon + app, like a real
    // session where the IME is its own client.
    let im_conn = Connection::connect_to_env().expect("im connect");
    let app_conn = Connection::connect_to_env().expect("app connect");
    let mut im_q = im_conn.new_event_queue();
    let mut app_q = app_conn.new_event_queue();
    let imqh = im_q.handle();
    let appqh = app_q.handle();
    let mut im = Im::default();
    let mut app = App::default();

    // IM claims the method first — real IME daemons start with
    // the session, before any app enables a field.
    let _ = im_conn.display().get_registry(&imqh, ());
    im_q.roundtrip(&mut im).unwrap();
    if im.mgr.is_none() {
        println!("RESULT missing zwp_input_method_manager_v2");
        std::process::exit(0);
    }
    let seat = im.seat.clone().expect("no seat");
    let imo = im.mgr.clone().unwrap().get_input_method(&seat, &imqh, ());
    im.im = Some(imo.clone());
    im_q.roundtrip(&mut im).unwrap();

    // App side: registry → toplevel → map → keyboard focus.
    let _ = app_conn.display().get_registry(&appqh, ());
    app_q.roundtrip(&mut app).unwrap();
    if app.tim.is_none() {
        println!("RESULT missing zwp_text_input_manager_v3");
        std::process::exit(0);
    }
    let surf = app.comp.clone().unwrap().create_surface(&appqh, ());
    let xs = app.wm.clone().unwrap().get_xdg_surface(&surf, &appqh, ());
    let tl = xs.get_toplevel(&appqh, ());
    tl.set_title("ti-test".into());
    let aseat = app.seat.clone().unwrap();
    let _kb = aseat.get_keyboard(&appqh, ());

    // 64x64 solid buffer — enough for the compositor to map us.
    let path = std::env::temp_dir().join(format!("odin-titest-{}", std::process::id()));
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
            .create_pool(bfd, 64 * 64 * 4, &appqh, ());
        let buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &appqh, ());
        surf.attach(Some(&buf), 0, 0);
        use std::io::Write;
        let mut wf = std::fs::File::options().write(true).open(&path).unwrap();
        wf.write_all(&vec![80u8, 40, 160, 255].repeat(64 * 64))
            .unwrap();
        surf.commit();
    }
    std::fs::remove_file(&path).ok();

    // text_input on our surface + seat — enter arrives with focus.
    let ti = app.tim.clone().unwrap().get_text_input(&aseat, &appqh, ());
    app.ti = Some(ti.clone());
    app_q.roundtrip(&mut app).unwrap();

    // Enable the field with surrounding text + cursor rect, commit
    // → the IM should activate and see the field state.
    ti.enable();
    ti.set_surrounding_text("hi".into(), 1, 1);
    ti.set_cursor_rectangle(5, 5, 20, 20);
    ti.commit();
    app_q.roundtrip(&mut app).unwrap();
    im_q.roundtrip(&mut im).unwrap();

    // IM composes and commits a string → flows to our text_input.
    if im.activated {
        imo.commit_string("OK".into());
        imo.commit(1);
    }
    im_q.roundtrip(&mut im).unwrap();
    app_q.roundtrip(&mut app).unwrap();

    // Field disabled → IM deactivates.
    ti.disable();
    ti.commit();
    app_q.roundtrip(&mut app).unwrap();
    im_q.roundtrip(&mut im).unwrap();

    println!(
        "RESULT ti_entered={} ti_commit={:?} ti_done={:?} im_act={} im_sur={:?} im_deact={}",
        app.entered, app.commit_str, app.done_serial, im.activated, im.surrounding, im.deactivated
    );
}
