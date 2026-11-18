// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Fullscreen commit-storm — the exact "menu overlay open" load:
//!
//!   zwlr_layer_surface on Overlay, anchored to all edges
//!   frame callback → attach + full-buffer damage + commit  (loop)
//!
//! Every commit damages the whole surface, forcing the compositor's
//! most expensive path each frame — the sustained load that made
//! the pointer feel slow while the launcher menu was open.
//!
//!   ODIN_BACKEND=nested ./odin &
//!   ./storm_proto_test
//!
//! Prints measured commit rate once a second; Ctrl+C exits.

use std::io::Write;
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1 as lsf,
};

#[derive(Default)]
struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    lshell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    output: Option<wl_output::WlOutput>,
    surf: Option<wl_surface::WlSurface>,
    cfg: Option<(u32, u32, u32)>,
    // Frame pacing state
    frame_pending: bool,
    frames: u64,
    last_report: Option<std::time::Instant>,
    tick: u32,
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
        if let lsf::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            ls.ack_configure(serial);
            app.cfg = Some((serial, width, height));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            app.frame_pending = false;
            app.frames += 1;
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
    wl_region::WlRegion,
    wl_shm_pool::WlShmPool,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    wl_output::WlOutput,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface
);

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App {
        last_report: Some(std::time::Instant::now()),
        ..Default::default()
    };
    let _reg = conn.display().get_registry(&qh, ());
    eq.roundtrip(&mut app).unwrap();

    // Fullscreen overlay — the launcher-menu scenario.
    // ODIN_LAYER=bottom|background|top picks another band and
    // ODIN_PASS_INPUT=1 empties the input region — together they
    // turn the storm into a background repaint meter for drag-
    // freeze tests: commits keep flowing under the windows while
    // all pointer input falls through to them.
    let pick = match std::env::var("ODIN_LAYER").as_deref() {
        Ok("background") => zwlr_layer_shell_v1::Layer::Background,
        Ok("bottom") => zwlr_layer_shell_v1::Layer::Bottom,
        Ok("top") => zwlr_layer_shell_v1::Layer::Top,
        _ => zwlr_layer_shell_v1::Layer::Overlay,
    };
    let surf = app.comp.as_ref().unwrap().create_surface(&qh, ());
    if std::env::var("ODIN_PASS_INPUT").is_ok() {
        let region = app.comp.as_ref().unwrap().create_region(&qh, ());
        surf.set_input_region(Some(&region));
    }
    let layer = app.lshell.as_ref().unwrap().get_layer_surface(
        &surf,
        app.output.as_ref(),
        pick,
        "storm".into(),
        &qh,
        (),
    );
    layer.set_anchor(
        lsf::Anchor::Top | lsf::Anchor::Bottom | lsf::Anchor::Left | lsf::Anchor::Right,
    );
    layer.set_size(0, 0);
    surf.commit();
    // Wait for the configure carrying the real size.
    while app.cfg.is_none() {
        eq.blocking_dispatch(&mut app).unwrap();
    }
    let (_, w, h) = app.cfg.unwrap();
    let (w, h) = (w.max(1), h.max(1));
    eprintln!("storm: configured {w}x{h} — committing on every frame callback");

    // One reusable shm buffer, repainted each frame.
    let path = std::env::temp_dir().join(format!("odin-storm-{}", std::process::id()));
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
        .as_ref()
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
    let mut mf = std::fs::File::options().write(true).open(&path).unwrap();

    app.surf = Some(surf);
    loop {
        // Repaint: moving horizontal band forces a real pixel change.
        app.tick = app.tick.wrapping_add(7);
        let mut px = vec![0u8; (w * h * 4) as usize];
        let band = (app.tick % (h as u32).max(1)) as usize;
        for y in 0..h as usize {
            let inside = (y as usize).abs_diff(band) < 40;
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                if inside {
                    px[i..i + 4].copy_from_slice(&[0x40, 0xff, 0x80, 0xff]);
                } else {
                    px[i..i + 4].copy_from_slice(&[0x18, 0x10, 0x28, 0xd8]);
                }
            }
        }
        use std::io::Seek;
        mf.seek(std::io::SeekFrom::Start(0)).unwrap();
        mf.write_all(&px).unwrap();
        let s = app.surf.as_ref().unwrap();
        s.attach(Some(&buf), 0, 0);
        s.damage(0, 0, w as i32, h as i32);
        s.frame(&qh, ());
        app.frame_pending = true;
        s.commit();
        while app.frame_pending {
            eq.blocking_dispatch(&mut app).unwrap();
        }
        let elapsed = app.last_report.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        if elapsed >= 1 {
            eprintln!("storm: {} commits/s", app.frames);
            app.frames = 0;
            app.last_report = Some(std::time::Instant::now());
        }
    }
}
