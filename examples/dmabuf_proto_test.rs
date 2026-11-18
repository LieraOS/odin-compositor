// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! linux-dmabuf protocol test — exports a memfd (which IS a
//! dmabuf) through `zwp_linux_dmabuf_v1`, both via `create`
//! (created/failed events) and `create_immed`. A client that
//! can't mmap-share pixels is indistinguishable from a broken
//! compositor path; this exercises the real one.

use std::os::fd::{AsFd, OwnedFd};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const W: i32 = 256;
const H: i32 = 256;
const FMT_XRGB: u32 = 0x3432_5258;

struct App {
    comp: Option<wl_compositor::WlCompositor>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    surface: Option<wl_surface::WlSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    formats: Vec<u32>,
    mods: u32,
    create_ok: bool,
    create_failed: bool,
    immed_failed: bool,
    configured: bool,
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
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    app.comp = Some(reg.bind(name, version.min(4), qh, ()));
                }
                "xdg_wm_base" => {
                    app.wm = Some(reg.bind(name, version.min(2), qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    app.dmabuf = Some(reg.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event as E;
        match event {
            E::Format { format } => {
                app.formats.push(format);
                println!("DMABUF-FORMAT {format:#x}");
            }
            E::Modifier { format, .. } => {
                app.mods += 1;
                println!("DMABUF-MOD {format:#x}");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { .. } => {
                app.create_ok = true;
                println!("DMABUF-CREATED");
            }
            zwp_linux_buffer_params_v1::Event::Failed => {
                app.create_failed = true;
                println!("DMABUF-FAILED");
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(App, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ())
    ]);
}

impl Dispatch<wl_compositor::WlCompositor, ()> for App {
    fn event(
        _app: &mut Self,
        _: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _app: &mut Self,
        _: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        _app: &mut Self,
        _: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _app: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        xs: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xs.ack_configure(serial);
            app.configured = true;
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        _app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Create a memfd filled with a recognizable pattern — a memfd
/// is a dmabuf, so this exercises the real export path.
fn make_pixels() -> OwnedFd {
    let fd = rustix::fs::memfd_create("dmabuf-test", rustix::fs::MemfdFlags::empty()).unwrap();
    rustix::fs::ftruncate(&fd, (W * H * 4) as u64).unwrap();
    let mut px = vec![0u8; (W * H * 4) as usize];
    for c in px.chunks_exact_mut(4) {
        c.copy_from_slice(&[0x00, 0x7f, 0x30, 0xff]); // BGRA orange
    }
    rustix::io::write(&fd, &px).unwrap();
    fd
}

fn main() {
    let conn = Connection::connect_to_env().expect("no compositor");
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut app = App {
        comp: None,
        wm: None,
        dmabuf: None,
        surface: None,
        toplevel: None,
        formats: Vec::new(),
        mods: 0,
        create_ok: false,
        create_failed: false,
        immed_failed: false,
        configured: false,
    };
    queue.roundtrip(&mut app).unwrap();
    queue.roundtrip(&mut app).unwrap();

    let comp = app.comp.clone().unwrap();
    let wm = app.wm.clone().unwrap();
    let dm = app.dmabuf.clone().unwrap();

    // Map a real toplevel so the dmabuf lands on screen.
    let surf = comp.create_surface(&qh, ());
    app.surface = Some(surf.clone());
    let xs = wm.get_xdg_surface(&surf, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    app.toplevel = Some(tl);
    surf.commit();
    queue.roundtrip(&mut app).unwrap();

    // Path 1: create_immed — buffer returns in the request.
    let fd = make_pixels();
    let p1 = dm.create_params(&qh, ());
    p1.add(fd.as_fd(), 0, 0, (W * 4) as u32, 0, 0);
    let buf = p1.create_immed(
        W,
        H,
        FMT_XRGB,
        zwp_linux_buffer_params_v1::Flags::empty(),
        &qh,
        (),
    );
    p1.destroy();
    queue.roundtrip(&mut app).unwrap();

    surf.attach(Some(&buf), 0, 0);
    surf.damage(0, 0, W, H);
    surf.commit();
    queue.roundtrip(&mut app).unwrap();

    // Path 2: create — the buffer arrives via the `created` event.
    let fd2 = make_pixels();
    let p2 = dm.create_params(&qh, ());
    p2.add(fd2.as_fd(), 0, 0, (W * 4) as u32, 0, 0);
    p2.create(W, H, FMT_XRGB, zwp_linux_buffer_params_v1::Flags::empty());
    // `create` makes the params inert but the object still
    // carries the created/failed events — destroy it AFTER
    // collecting, or the event dies with the proxy.
    for _ in 0..3 {
        queue.roundtrip(&mut app).unwrap();
    }
    p2.destroy();

    // Negative check: zero planes must come back `failed`.
    let p3 = dm.create_params(&qh, ());
    p3.create(W, H, FMT_XRGB, zwp_linux_buffer_params_v1::Flags::empty());
    for _ in 0..3 {
        queue.roundtrip(&mut app).unwrap();
    }
    p3.destroy();

    println!(
        "RESULT ok={}",
        app.create_ok && app.create_failed && !app.immed_failed
    );
}
