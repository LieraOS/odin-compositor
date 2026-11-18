// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pointer-extension protocol test client — binds the three
//! pointer globals, maps a surface, and verifies that
//! relative-motion and confine events actually flow.
//!
//! Run inside a nested odin: WAYLAND_DISPLAY=odin-0
//! Prints RELMOTION/CONFINED/SHAPE lines for grep-checking.

use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1,
    zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::ZwpRelativePointerV1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct App {
    seat: Option<wl_seat::WlSeat>,
    relman: Option<ZwpRelativePointerManagerV1>,
    conman: Option<ZwpPointerConstraintsV1>,
    shapeman: Option<WpCursorShapeManagerV1>,
    gestures: Option<
        wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
    >,
    shm: Option<wl_shm::WlShm>,
    comp: Option<wl_compositor::WlCompositor>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    got_confined: bool,
    got_rel: bool,
    // Gesture event counters — a full begin/update/end stream
    // lands whenever the sweep's pointer sits over our surface.
    swipe_begins: u32,
    swipe_ends: u32,
    pinch_begins: u32,
    pinch_ends: u32,
    // Touch counters — the selftest injects a down/motion/up
    // point at the swept position; crossing our surface must
    // yield a full down→motion→up→frame batch.
    touch_downs: u32,
    touch_motions: u32,
    touch_ups: u32,
    touch_frames: u32,
}

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        reg: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event as E;
        if let E::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    app.comp = Some(reg.bind(name, 4, qh, ()));
                }
                "wl_shm" => {
                    app.shm = Some(reg.bind(name, 1, qh, ()));
                }
                "xdg_wm_base" => {
                    app.wm = Some(reg.bind(name, 3, qh, ()));
                }
                "wl_seat" => {
                    app.seat = Some(reg.bind(name, 8, qh, ()));
                }
                "zwp_relative_pointer_manager_v1" => {
                    app.relman = Some(reg.bind(name, 1, qh, ()));
                    println!("BOUND relative-pointer");
                }
                "zwp_pointer_constraints_v1" => {
                    app.conman = Some(reg.bind(name, 1, qh, ()));
                    println!("BOUND pointer-constraints");
                }
                "wp_cursor_shape_manager_v1" => {
                    app.shapeman = Some(reg.bind(name, 1, qh, ()));
                    println!("BOUND cursor-shape");
                }
                "zwp_pointer_gestures_v1" => {
                    app.gestures = Some(reg.bind(name, 3, qh, ()));
                    println!("BOUND pointer-gestures");
                }
                _ => {}
            }
        }
    }
}

// delegate_noop explodes on objects that actually emit events —
// every interface that talks back gets a real no-op handler.
macro_rules! noop {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
noop!(
    wl_seat::WlSeat,
    wl_pointer::WlPointer,
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    xdg_wm_base::XdgWmBase,
    xdg_toplevel::XdgToplevel,
    ZwpRelativePointerManagerV1,
    ZwpPointerConstraintsV1,
    WpCursorShapeManagerV1,
    WpCursorShapeDeviceV1,
    wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1
);

// Gesture objects talk back — real handlers that count the
// begin/end pairs the selftest's injected streams produce.
impl Dispatch<
    wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1,
    (),
> for App {
    fn event(
        app: &mut Self,
        _: &wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1,
        event: wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_swipe_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_swipe_v1::Event as E;
        match event {
            E::Begin { .. } => app.swipe_begins += 1,
            E::End { .. } => app.swipe_ends += 1,
            _ => {}
        }
    }
}

impl Dispatch<
    wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
    (),
> for App {
    fn event(
        app: &mut Self,
        _: &wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
        event: wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_pinch_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_pinch_v1::Event as E;
        match event {
            E::Begin { .. } => app.pinch_begins += 1,
            E::End { .. } => app.pinch_ends += 1,
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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

impl Dispatch<ZwpRelativePointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::Event as E;
        if let E::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            println!("RELMOTION dx={dx:.1} dy={dy:.1} ux={dx_unaccel:.1} uy={dy_unaccel:.1}");
            app.got_rel = true;
        }
    }
}

// wl_touch — the only client-side touch object; the selftest's
// injected slots land here when the sweep crosses our surface.
impl Dispatch<wayland_client::protocol::wl_touch::WlTouch, ()> for App {
    fn event(
        app: &mut Self,
        _: &wayland_client::protocol::wl_touch::WlTouch,
        event: wayland_client::protocol::wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_touch::Event as E;
        match event {
            E::Down { .. } => app.touch_downs += 1,
            E::Motion { .. } => app.touch_motions += 1,
            E::Up { .. } => app.touch_ups += 1,
            E::Frame => app.touch_frames += 1,
            _ => {}
        }
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpConfinedPointerV1,
        event: wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::Event as E;
        match event {
            E::Confined => {
                println!("CONFINED");
                app.got_confined = true;
            }
            E::Unconfined => println!("UNCONFINED"),
            _ => {}
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect");
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App {
        seat: None,
        relman: None,
        conman: None,
        shapeman: None,
        gestures: None,
        shm: None,
        comp: None,
        wm: None,
        got_confined: false,
        got_rel: false,
        swipe_begins: 0,
        swipe_ends: 0,
        pinch_begins: 0,
        pinch_ends: 0,
        touch_downs: 0,
        touch_motions: 0,
        touch_ups: 0,
        touch_frames: 0,
    };
    let _ = display.get_registry(&qh, ());
    eq.roundtrip(&mut app).expect("roundtrip");

    // A real mapped surface — pointer focus needs something to
    // hit, and confine needs a surface to constrain to.
    let surf = app.comp.take().unwrap().create_surface(&qh, ());
    let xs = app.wm.take().unwrap().get_xdg_surface(&surf, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    tl.set_title("ptr-test".into());
    // 200x200 ARGB fill via shm pool.
    let (w, h) = (200u32, 200u32);
    let path = std::env::temp_dir().join(format!("odin-ptrtest-{}", std::process::id()));
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len((w * h * 4) as u64).unwrap();
        // SAFETY: `f` outlives the call; borrow_raw only borrows.
        let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(f.as_raw_fd()) };
        let pool = app
            .shm
            .take()
            .unwrap()
            .create_pool(bfd, (w * h * 4) as i32, &qh, ());
        let buf = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            (w * 4) as i32,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        surf.attach(Some(&buf), 0, 0);
        // Fill red via mmap on the client side.
        {
            use std::io::Write;
            let mut f2 = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let px = vec![0xffu8, 0x00, 0x00, 0xff];
            let data = px.repeat((w * h) as usize);
            f2.write_all(&data).unwrap();
        }
        surf.commit();
    }
    std::fs::remove_file(&path).ok();

    let seat = app.seat.take().unwrap();
    let ptr = seat.get_pointer(&qh, ());
    // Relative pointer on the same wl_pointer.
    let _relp = app
        .relman
        .take()
        .unwrap()
        .get_relative_pointer(&ptr, &qh, ());
    // Persistent confine over the whole surface.
    let _conf = app
        .conman
        .take()
        .unwrap()
        .confine_pointer(&surf, &ptr, None, wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::Lifetime::Persistent, &qh, ());
    // A themed cursor — proves the shape path end to end.
    let dev = app.shapeman.take().unwrap().get_pointer(&ptr, &qh, ());
    dev.set_shape(
        0,
        wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape::Pointer,
    );
    // Gesture objects on the same pointer — the selftest
    // injects swipe+pinch streams during the sweep; only the
    // focused client's objects may fire.
    if let Some(g) = app.gestures.take() {
        let _sw = g.get_swipe_gesture(&ptr, &qh, ());
        let _pi = g.get_pinch_gesture(&ptr, &qh, ());
    }
    // wl_touch on the same seat — the selftest injects touch
    // points during the sweep; crossing our surface yields a
    // complete down/motion/up/frame stream.
    let _touch = seat.get_touch(&qh, ());
    println!("SHAPE set");
    conn.flush().unwrap();

    // Pump events until we saw a relative motion or ~25s pass.
    let start = std::time::Instant::now();
    while !app.got_rel && start.elapsed().as_secs() < 25 {
        let _ = eq.blocking_dispatch(&mut app);
    }
    // Keep pumping briefly after rel-motion so the sweep still
    // has time to cross our surface with a gesture stream.
    let gstart = std::time::Instant::now();
    while (app.swipe_begins == 0 || app.pinch_begins == 0) && gstart.elapsed().as_secs() < 10 {
        let _ = eq.blocking_dispatch(&mut app);
    }
    // A few more pumps so the injected touch batch can land
    // before we report.
    let tstart = std::time::Instant::now();
    while app.touch_downs == 0 && tstart.elapsed().as_secs() < 10 {
        let _ = eq.blocking_dispatch(&mut app);
    }
    println!(
        "RESULT rel={} confined={} swipe={}/{} pinch={}/{} touch={}/{}/{}/{}",
        app.got_rel,
        app.got_confined,
        app.swipe_begins,
        app.swipe_ends,
        app.pinch_begins,
        app.pinch_ends,
        app.touch_downs,
        app.touch_motions,
        app.touch_ups,
        app.touch_frames
    );
}
