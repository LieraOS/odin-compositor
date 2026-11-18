// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Drag & drop protocol test client — maps a real xdg
//! toplevel, then plays BOTH sides of a drag onto itself:
//!
//! 1. a wl_pointer.button press triggers data_device.start_drag
//! 2. the compositor re-enters the same surface as drop target
//! 3. we accept a mime, set actions, drop, receive the data fd,
//!    read it, and call finish
//!
//! Prints DND- lines for grep-checking; RESULT ok=true when the
//! source saw target/action/drop_performed/finished AND the
//! destination got enter/drop/data back.

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
    wl_data_source, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const PAYLOAD: &[u8] = b"odin-dnd-payload";

struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    surface: Option<wl_surface::WlSurface>,
    device: Option<wl_data_device::WlDataDevice>,
    source: Option<wl_data_source::WlDataSource>,
    offer: Option<wl_data_offer::WlDataOffer>,
    drag_started: bool,
    pressed_serial: u32,
    recv_fd: Option<OwnedFd>,
    finished_offer: bool,
    saw_enter: bool,
    saw_drop: bool,
    got_data: String,
    saw_target: bool,
    saw_action: bool,
    saw_performed: bool,
    saw_finished: bool,
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
                "wl_compositor" => app.comp = Some(reg.bind(name, 1, qh, ())),
                "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" => app.seat = Some(reg.bind(name, 1, qh, ())),
                "wl_data_device_manager" => app.ddm = Some(reg.bind(name, 3, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button { serial, state, .. } = event {
            // The press over our own surface starts the drag —
            // exactly what a toolkit does on drag-begin.
            if state == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed)
                && !app.drag_started
            {
                app.drag_started = true;
                app.pressed_serial = serial;
                if let (Some(dev), Some(src), Some(surf)) = (&app.device, &app.source, &app.surface)
                {
                    dev.start_drag(Some(src), surf, None, serial);
                    println!("DND-START serial={serial}");
                }
            }
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    // data_offer events carry a new_id — the child's ObjectData
    // must come from this specialization.
    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<wl_data_offer::WlDataOffer, ()>(()),
            _ => unreachable!(),
        }
    }

    fn event(
        app: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                println!("DND-OFFER");
                app.offer = Some(id);
            }
            wl_data_device::Event::Enter { serial, .. } => {
                println!("DND-ENTER");
                app.saw_enter = true;
                // Negotiate like a real destination: accept a
                // mime and pick a preferred action.
                if let Some(offer) = &app.offer {
                    offer.accept(serial, Some("text/plain".into()));
                    offer.set_actions(
                        wl_data_device_manager::DndAction::Copy,
                        wl_data_device_manager::DndAction::Copy,
                    );
                }
            }
            wl_data_device::Event::Drop { .. } => {
                println!("DND-DROP");
                app.saw_drop = true;
                // Pull the payload through a real fd — receive
                // hands it to the source's send event; the data
                // lands after the next dispatch rounds.
                if let Some(offer) = &app.offer {
                    let fd = rustix::fs::memfd_create("dnd-recv", rustix::fs::MemfdFlags::empty())
                        .unwrap();
                    offer.receive("text/plain".into(), fd.as_fd());
                    app.recv_fd = Some(fd);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            println!("DND-MIME {mime_type}");
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use std::io::Write;
        match event {
            wl_data_source::Event::Target { .. } => {
                println!("DND-TARGET");
                app.saw_target = true;
            }
            wl_data_source::Event::Action { .. } => {
                println!("DND-ACTION");
                app.saw_action = true;
            }
            wl_data_source::Event::Send { mime_type, fd } => {
                println!("DND-SEND {mime_type}");
                std::fs::File::from(fd).write_all(PAYLOAD).ok();
            }
            wl_data_source::Event::DndDropPerformed => {
                println!("DND-PERFORMED");
                app.saw_performed = true;
            }
            wl_data_source::Event::DndFinished => {
                println!("DND-FINISHED");
                app.saw_finished = true;
            }
            wl_data_source::Event::Cancelled => {
                println!("DND-CANCELLED");
            }
            _ => {}
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
    xdg_wm_base::XdgWmBase,
    xdg_toplevel::XdgToplevel,
    wl_data_device_manager::WlDataDeviceManager
);

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let display = conn.display();
    let _ = display.get_registry(&qh, ());
    let mut app = App {
        comp: None,
        shm: None,
        wm: None,
        seat: None,
        ddm: None,
        surface: None,
        device: None,
        source: None,
        offer: None,
        drag_started: false,
        pressed_serial: 0,
        recv_fd: None,
        finished_offer: false,
        saw_enter: false,
        saw_drop: false,
        got_data: String::new(),
        saw_target: false,
        saw_action: false,
        saw_performed: false,
        saw_finished: false,
    };
    eq.roundtrip(&mut app).unwrap();

    // A real mapped toplevel — the drag needs a hittable window.
    let surf = app.comp.clone().unwrap().create_surface(&qh, ());
    let xs = app.wm.clone().unwrap().get_xdg_surface(&surf, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    tl.set_title("dnd-test".into());
    let (w, h) = (200u32, 200u32);
    let path = std::env::temp_dir().join(format!("odin-dndtest-{}", std::process::id()));
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len((w * h * 4) as u64).unwrap();
        let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(f.as_raw_fd()) };
        let pool = app
            .shm
            .clone()
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
        surf.commit();
    }
    std::fs::remove_file(&path).ok();
    app.surface = Some(surf);

    // Seat pointer + data plumbing — both sides of the drag.
    let _ptr = app.seat.clone().unwrap().get_pointer(&qh, ());
    let dev = app
        .ddm
        .clone()
        .unwrap()
        .get_data_device(app.seat.as_ref().unwrap(), &qh, ());
    app.device = Some(dev);
    let src = app.ddm.clone().unwrap().create_data_source(&qh, ());
    src.offer("text/plain".into());
    app.source = Some(src);

    // Event loop — drag lifecycle messages keep arriving until
    // the compositor exits the selftest.
    let mut tries = 0;
    loop {
        if eq.blocking_dispatch(&mut app).is_err() {
            break;
        }
        // The source's write lands a few dispatches after our
        // receive request — retry until data shows up, then
        // finish the offer to close out the drag.
        if let Some(fd) = &app.recv_fd {
            if !app.finished_offer {
                let mut f = std::fs::File::from(fd.try_clone().unwrap());
                use std::io::Seek;
                f.seek(std::io::SeekFrom::Start(0)).ok();
                let mut s = String::new();
                f.read_to_string(&mut s).ok();
                if !s.is_empty() {
                    app.got_data = s.clone();
                    println!("DND-DATA {s}");
                    if let Some(offer) = &app.offer {
                        offer.finish();
                        app.finished_offer = true;
                    }
                } else {
                    tries += 1;
                }
            }
        }
        // After the source finished, score the round-trip.
        if app.saw_finished {
            let ok = app.saw_enter
                && app.saw_drop
                && app.saw_target
                && app.saw_action
                && app.saw_performed
                && app.got_data.as_bytes() == PAYLOAD;
            println!("RESULT ok={ok}");
            break;
        }
        if tries > 200 {
            println!("RESULT ok=false (no data)");
            break;
        }
    }
}
