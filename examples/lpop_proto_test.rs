// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Layer-popup end-to-end — the exact GTK menu pattern that
//! used to kill the compositor:
//!
//!   xdg_surface.get_popup(parent = NULL, positioner)
//!   zwlr_layer_surface.get_popup(popup)   ← reparent to layer
//!   popup surface attach + commit          → must render
//!
//! Before the fix, get_popup(NULL) left the xdg_popup without
//! object data — the next request on it was a hard server panic.
//! Survival + a rendered popup is the proof.
//!
//!   ODIN_BACKEND=nested ./odin &
//!   ./lpop_proto_test

use std::io::Write;
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1 as lsf,
};

fn make_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<App>,
    w: u32,
    h: u32,
    rgba: [u8; 4],
) -> wl_buffer::WlBuffer {
    let path = std::env::temp_dir().join(format!("odin-lpop-{}", std::process::id()));
    // O_RDWR — the server mmaps PROT_READ|PROT_WRITE.
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
    lshell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    output: Option<wl_output::WlOutput>,
    layer_serial: Option<u32>,
    layer_cfg_done: bool,
    popup_cfg: Option<(i32, i32, i32, i32)>,
    popup_done: bool,
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
                "wl_compositor" => app.comp = Some(reg.bind(name, version.min(5), qh, ())),
                "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(reg.bind(name, 3, qh, ())),
                "zwlr_layer_shell_v1" => app.lshell = Some(reg.bind(name, 4, qh, ())),
                "wl_output" => app.output = Some(reg.bind(name, version.min(4), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<lsf::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        ls: &lsf::ZwlrLayerSurfaceV1,
        event: lsf::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let lsf::Event::Configure { serial, .. } = event {
            ls.ack_configure(serial);
            app.layer_cfg_done = true;
            app.layer_serial = Some(serial);
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                app.popup_cfg = Some((x, y, width, height));
                println!("POPUP-CONFIGURE {x},{y} {width}x{height}");
            }
            xdg_popup::Event::PopupDone => app.popup_done = true,
            _ => {}
        }
    }
}

macro_rules! silent {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

silent!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    xdg_wm_base::XdgWmBase,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    wl_output::WlOutput,
    wl_surface::WlSurface,
    wl_buffer::WlBuffer,
    xdg_surface::XdgSurface,
    xdg_positioner::XdgPositioner
);

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App::default();
    let _reg = conn.display().get_registry(&qh, ());
    eq.roundtrip(&mut app).unwrap();

    // Layer surface — top band, bottom-anchored, taskbar-like.
    let surf1 = app.comp.as_ref().unwrap().create_surface(&qh, ());
    let layer = app.lshell.as_ref().unwrap().get_layer_surface(
        &surf1,
        app.output.as_ref(),
        zwlr_layer_shell_v1::Layer::Top,
        "testbar".into(),
        &qh,
        (),
    );
    layer.set_anchor(lsf::Anchor::Bottom | lsf::Anchor::Left | lsf::Anchor::Right);
    layer.set_size(0, 50);
    surf1.commit();
    eq.roundtrip(&mut app).unwrap();
    assert!(app.layer_cfg_done, "layer surface never configured");

    let bar = make_buffer(app.shm.as_ref().unwrap(), &qh, 1280, 50, [40, 40, 40, 255]);
    surf1.attach(Some(&bar), 0, 0);
    surf1.commit();
    eq.roundtrip(&mut app).unwrap();
    println!("LAYER-UP");

    // The GTK menu sequence: popup with NULL parent, then the
    // layer surface claims it — this is what killed the server.
    let surf2 = app.comp.as_ref().unwrap().create_surface(&qh, ());
    let xsurf = app.wm.as_ref().unwrap().get_xdg_surface(&surf2, &qh, ());
    let pos = app.wm.as_ref().unwrap().create_positioner(&qh, ());
    pos.set_size(300, 400);
    pos.set_anchor_rect(0, 0, 100, 50);
    pos.set_anchor(xdg_positioner::Anchor::TopLeft);
    let popup = xsurf.get_popup(None, &pos, &qh, ());
    layer.get_popup(&popup);
    eq.roundtrip(&mut app).unwrap();
    println!("POPUP-CREATED cfg={:?}", app.popup_cfg);

    // Commit content on the popup — menu must be able to draw.
    let menu = make_buffer(app.shm.as_ref().unwrap(), &qh, 300, 400, [30, 30, 200, 255]);
    surf2.attach(Some(&menu), 0, 0);
    surf2.commit();
    eq.roundtrip(&mut app).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    eq.roundtrip(&mut app).unwrap();
    // popup_done must NOT have fired — it is dismissal, and the
    // compositor may only send it on outside click or unmap.
    println!(
        "RESULT alive=true popup_cfg={:?} popup_done={}",
        app.popup_cfg.is_some(),
        app.popup_done
    );
}
