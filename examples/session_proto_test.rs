// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Session-protocol test client — exercises the three pieces a
//! desktop session needs that a plain window never touches:
//!
//!   1. ext_idle_notifier_v1 — arm a short timeout, wait for
//!      `idled`, then inject real motion through a
//!      zwlr_virtual_pointer and expect `resumed`
//!   2. xdg_activation_v1 — commit a token, activate a second
//!      toplevel, expect its keyboard focus back
//!   3. ext_session_lock_v1 — lock, supply a lock surface,
//!      verify via log/screencopy that only it renders, then
//!      unlock_and_destroy and expect `finished`
//!
//! Prints SESSION- lines for grep-checking; RESULT ok=… ends it.

use std::io::Write;
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1 as ein, ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1, ext_session_lock_surface_v1 as ess,
    ext_session_lock_v1 as esl,
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1 as xat, xdg_activation_v1::XdgActivationV1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1 as zvp,
};

/// Grow a solid-color shm buffer — the lock surface wants
/// output-sized pixels; the test paints a signal red.
fn make_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<App>,
    w: u32,
    h: u32,
    rgba: [u8; 4],
) -> wl_buffer::WlBuffer {
    let path = std::env::temp_dir().join(format!("odin-sesstest-{}", std::process::id()));
    // O_RDWR is required — the server mmaps the fd PROT_READ |
    // PROT_WRITE; a write-only File::create fd fails with EACCES
    // and the buffer maps to MAP_FAILED (silent black output).
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    f.set_len((w * h * 4) as u64).unwrap();
    {
        let mut mf = std::fs::File::options().write(true).open(&path).unwrap();
        let row = rgba.repeat(w as usize);
        for _ in 0..h {
            mf.write_all(&row).unwrap();
        }
    }
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(f.as_raw_fd()) };
    let pool = shm.create_pool(bfd, (w * h * 4) as i32, qh, ());
    let buf = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        (w * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    std::fs::remove_file(&path).ok();
    buf
}

#[derive(Default)]
struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    output: Option<wl_output::WlOutput>,
    notifier: Option<ExtIdleNotifierV1>,
    lock_mgr: Option<ExtSessionLockManagerV1>,
    activation: Option<XdgActivationV1>,
    vptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    // test state
    saw_idled: bool,
    saw_resumed: bool,
    kb_surface: String,
    focus_a: bool,
    focus_b: bool,
    token_done: Option<String>,
    lock_locked: bool,
    lock_finished: bool,
    lock_cfg: Option<(u32, u32, u32)>,
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
                "wl_output" => {
                    if app.output.is_none() {
                        app.output = Some(reg.bind(name, 4, qh, ()))
                    }
                }
                "ext_idle_notifier_v1" => app.notifier = Some(reg.bind(name, 2, qh, ())),
                "ext_session_lock_manager_v1" => app.lock_mgr = Some(reg.bind(name, 1, qh, ())),
                "xdg_activation_v1" => app.activation = Some(reg.bind(name, 1, qh, ())),
                "zwlr_virtual_pointer_manager_v1" => app.vptr_mgr = Some(reg.bind(name, 2, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ein::ExtIdleNotificationV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ein::ExtIdleNotificationV1,
        event: ein::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ein::Event::Idled => {
                println!("SESSION-IDLED");
                app.saw_idled = true;
            }
            ein::Event::Resumed => {
                println!("SESSION-RESUMED");
                app.saw_resumed = true;
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
        if let wl_keyboard::Event::Enter { surface, .. } = event {
            println!("SESSION-KBENTER surf={}", surface.id());
            app.kb_surface = format!("{}", surface.id());
        }
    }
}

impl Dispatch<xat::XdgActivationTokenV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &xat::XdgActivationTokenV1,
        event: xat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xat::Event::Done { token } = event {
            println!("SESSION-TOKEN {token}");
            app.token_done = Some(token);
        }
    }
}

impl Dispatch<esl::ExtSessionLockV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &esl::ExtSessionLockV1,
        event: esl::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            esl::Event::Locked => {
                println!("SESSION-LOCK-LOCKED");
                app.lock_locked = true;
            }
            esl::Event::Finished => {
                println!("SESSION-LOCK-FINISHED");
                app.lock_finished = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ess::ExtSessionLockSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        obj: &ess::ExtSessionLockSurfaceV1,
        event: ess::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ess::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            println!("SESSION-LOCK-CONFIGURE {width}x{height}");
            obj.ack_configure(serial);
            app.lock_cfg = Some((serial, width, height));
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
    wl_seat::WlSeat,
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wl_output::WlOutput,
    xdg_wm_base::XdgWmBase,
    xdg_toplevel::XdgToplevel,
    ExtIdleNotifierV1,
    ExtSessionLockManagerV1,
    XdgActivationV1,
    ZwlrVirtualPointerManagerV1,
    zvp::ZwlrVirtualPointerV1
);

fn map_toplevel(
    app: &App,
    qh: &QueueHandle<App>,
    title: &str,
    color: [u8; 4],
) -> wl_surface::WlSurface {
    let surf = app.comp.clone().unwrap().create_surface(qh, ());
    let xs = app.wm.clone().unwrap().get_xdg_surface(&surf, qh, ());
    let tl = xs.get_toplevel(qh, ());
    tl.set_title(title.into());
    let buf = make_buffer(app.shm.as_ref().unwrap(), qh, 120, 90, color);
    surf.attach(Some(&buf), 0, 0);
    surf.commit();
    surf
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _ = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    eq.roundtrip(&mut app).unwrap();
    let _kb = app.seat.clone().unwrap().get_keyboard(&qh, ());

    // ── Phase 1: idle notify ────────────────────────────────
    let _notif = app.notifier.clone().unwrap().get_idle_notification(
        700,
        &app.seat.clone().unwrap(),
        &qh,
        (),
    );
    // No input for 700ms → idled must fire. blocking_dispatch
    // sleeps until the server speaks — which is exactly the
    // event we wait for.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while !app.saw_idled && std::time::Instant::now() < deadline {
        eq.blocking_dispatch(&mut app).unwrap();
    }

    // Virtual pointer injects real motion → resumed.
    let vptr = app
        .vptr_mgr
        .clone()
        .unwrap()
        .create_virtual_pointer(None, &qh, ());
    vptr.motion(0, 3.0, 2.0);
    vptr.frame();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while !app.saw_resumed && std::time::Instant::now() < deadline {
        eq.blocking_dispatch(&mut app).unwrap();
    }

    // ── Phase 2: activation ─────────────────────────────────
    // Two toplevels — B maps after A and takes focus; the token
    // activation must pull focus back to A.
    let surf_a = map_toplevel(&app, &qh, "sess-A", [40, 40, 200, 255]);
    let surf_a_id = format!("{}", surf_a.id());
    eq.roundtrip(&mut app).unwrap();
    let _surf_b = map_toplevel(&app, &qh, "sess-B", [40, 200, 40, 255]);
    eq.roundtrip(&mut app).unwrap();
    eq.roundtrip(&mut app).unwrap();
    app.focus_b = app.kb_surface != surf_a_id;

    let token_obj = app
        .activation
        .clone()
        .unwrap()
        .get_activation_token(&qh, ());
    token_obj.set_app_id("session-proto-test".into());
    token_obj.commit();
    eq.roundtrip(&mut app).unwrap();
    if let Some(tok) = app.token_done.clone() {
        app.activation.clone().unwrap().activate(tok, &surf_a);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while app.kb_surface != surf_a_id && std::time::Instant::now() < deadline {
            eq.blocking_dispatch(&mut app).unwrap();
        }
        app.focus_a = app.kb_surface == surf_a_id;
        println!("SESSION-ACTIVATE refocused={}", app.focus_a);
    }

    // ── Phase 3: session lock ───────────────────────────────
    let lock = app.lock_mgr.clone().unwrap().lock(&qh, ());
    eq.roundtrip(&mut app).unwrap();
    if app.lock_locked {
        let lock_surf = app.comp.clone().unwrap().create_surface(&qh, ());
        let _ls = lock.get_lock_surface(&lock_surf, app.output.as_ref().unwrap(), &qh, ());
        // Wait for the configure, then supply the output-sized
        // buffer the protocol demands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while app.lock_cfg.is_none() && std::time::Instant::now() < deadline {
            eq.blocking_dispatch(&mut app).unwrap();
        }
        if let Some((_, w, h)) = app.lock_cfg {
            let buf = make_buffer(app.shm.as_ref().unwrap(), &qh, w, h, [200, 30, 30, 255]);
            lock_surf.attach(Some(&buf), 0, 0);
            lock_surf.commit();
            // Requests stay in the client-side send queue until a
            // flush — sleeping alone does NOT send them. Without
            // this the compositor sees attach/commit/unlock as one
            // batch after the sleep and the lock surface never
            // renders.
            eq.flush().unwrap();
            println!("SESSION-LOCK-SURFACE-UP");
            // Hold the lock briefly — the harness screenshots
            // the output in this window to prove only the lock
            // surface renders.
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        lock.unlock_and_destroy();
        // unlock_and_destroy is a destructor — our lock object's
        // proxy is already dead when `finished` arrives, so the
        // client can't observe it. The unlock proof is instead:
        // a fresh lock() must get `locked` again immediately.
        app.lock_locked = false;
        let _lock2 = app.lock_mgr.clone().unwrap().lock(&qh, ());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while !app.lock_locked && std::time::Instant::now() < deadline {
            eq.blocking_dispatch(&mut app).unwrap();
        }
        app.lock_finished = app.lock_locked;
    }

    let ok = app.saw_idled
        && app.saw_resumed
        && app.focus_b
        && app.focus_a
        && app.lock_locked
        && app.lock_cfg.is_some()
        && app.lock_finished;
    println!(
        "RESULT ok={ok} idled={} resumed={} focus={}/{} lock={}/{}/{}",
        app.saw_idled,
        app.saw_resumed,
        app.focus_b,
        app.focus_a,
        app.lock_locked,
        app.lock_cfg.is_some(),
        app.lock_finished
    );
}
