// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wp_viewporter protocol test — proves the crop+scale path
//! end-to-end through the compositor's composite, not just the
//! request handshake:
//!
//!   1. fullscreen toplevel with a two-tone buffer
//!      (left green, right blue)
//!   2. zwlr_screencopy capture → right half must be BLUE
//!   3. viewport: src = left half, dst = full size → commit
//!      → capture → right half must be GREEN (crop stretched)
//!   4. unset viewport → capture → right half BLUE again
//!
//! Fullscreen pins the window to the output origin so the
//! captured pixel positions are deterministic. Prints RESULT.

use std::io::Write;
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, wp_fractional_scale_v1 as wfs,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport as wpv, wp_viewporter::WpViewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_wm_base};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as scf, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// BGRA halves — left green, right blue. Chosen so a stretched
/// left half can't be mistaken for the right one by any single
/// channel.
const GREEN: [u8; 4] = [0, 200, 0, 255];
const BLUE: [u8; 4] = [200, 0, 0, 255];

/// Pool file kept open — the compositor composites the
/// screencopy straight into our shm, so readback needs the fd.
fn shm_file(size: u64) -> (std::fs::File, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("odin-vptest-{}", std::process::id()));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    f.set_len(size).unwrap();
    (f, path)
}

/// Two-tone shm buffer — left half `l`, right half `r`.
fn two_tone(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<App>,
    w: u32,
    h: u32,
    l: [u8; 4],
    r: [u8; 4],
) -> wl_buffer::WlBuffer {
    let (f, path) = shm_file((w * h * 4) as u64);
    {
        let mut mf = std::fs::File::options().write(true).open(&path).unwrap();
        for _y in 0..h {
            let row: Vec<u8> = l
                .repeat((w / 2) as usize)
                .into_iter()
                .chain(r.repeat((w - w / 2) as usize))
                .collect();
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
    output: Option<wl_output::WlOutput>,
    vptr: Option<WpViewporter>,
    fs_mgr: Option<WpFractionalScaleManagerV1>,
    scr: Option<ZwlrScreencopyManagerV1>,
    // preferred_scale announcement — ×120 fixed point
    fs_scale: Option<u32>,
    // configure handshake — fullscreen size comes from here
    cfg: Option<(u32, u32)>,
    // screencopy offer → we create this buffer and copy into it
    scr_offer: Option<(u32, u32, u32)>, // w, h, stride
    scr_ready: bool,
    scr_failed: bool,
    frame_done: bool,
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
                "wl_output" => {
                    if app.output.is_none() {
                        app.output = Some(reg.bind(name, 4, qh, ()))
                    }
                }
                "wp_viewporter" => app.vptr = Some(reg.bind(name, 1, qh, ())),
                "wp_fractional_scale_manager_v1" => app.fs_mgr = Some(reg.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => app.scr = Some(reg.bind(name, 3, qh, ())),
                _ => {}
            }
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

impl Dispatch<wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel,
        event: wayland_protocols::xdg::shell::client::xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_protocols::xdg::shell::client::xdg_toplevel::Event::Configure {
            width,
            height,
            ..
        } = event
        {
            if width > 0 && height > 0 {
                println!("VP-CONFIGURE {width}x{height}");
                app.cfg = Some((width as u32, height as u32));
            }
        }
    }
}

impl Dispatch<scf::ZwlrScreencopyFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        frame: &scf::ZwlrScreencopyFrameV1,
        event: scf::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            scf::Event::Buffer {
                width,
                height,
                stride,
                ..
            } => {
                app.scr_offer = Some((width, height, stride));
            }
            scf::Event::Ready { .. } => {
                app.scr_ready = true;
            }
            scf::Event::Failed => {
                app.scr_failed = true;
            }
            _ => {}
        }
        let _ = frame;
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for App {
    fn event(
        app: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        event: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            app.frame_done = true;
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
impl Dispatch<wfs::WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &wfs::WpFractionalScaleV1,
        event: wfs::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wfs::Event::PreferredScale { scale } = event {
            println!("VP-FS-SCALE {scale}");
            app.fs_scale = Some(scale);
        }
    }
}

noop!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wl_output::WlOutput,
    xdg_wm_base::XdgWmBase,
    WpViewporter,
    wpv::WpViewport,
    WpFractionalScaleManagerV1,
    ZwlrScreencopyManagerV1
);

/// Wait until `pred` holds, dispatching events — bounded so a
/// broken compositor can't hang the suite.
fn wait_for(
    eq: &mut wayland_client::EventQueue<App>,
    app: &mut App,
    secs: u64,
    pred: impl Fn(&App) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !pred(app) && std::time::Instant::now() < deadline {
        if eq.blocking_dispatch(app).is_err() {
            break;
        }
    }
    pred(app)
}

/// One capture: request → offer → allocate + copy → ready →
/// read the pixel at (px, py) out of our pool file (BGRA).
fn capture_px(
    eq: &mut wayland_client::EventQueue<App>,
    app: &mut App,
    qh: &QueueHandle<App>,
    px: u32,
    py: u32,
) -> Option<[u8; 4]> {
    app.scr_offer = None;
    app.scr_ready = false;
    app.scr_failed = false;
    let frame = app
        .scr
        .clone()
        .unwrap()
        .capture_output(0, app.output.as_ref().unwrap(), qh, ());
    if !wait_for(eq, app, 4, |a| a.scr_offer.is_some() || a.scr_failed) {
        return None;
    }
    let (w, h, stride) = app.scr_offer?;
    let (f, path) = shm_file((stride * h) as u64);
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(f.as_raw_fd()) };
    let pool = app
        .shm
        .clone()
        .unwrap()
        .create_pool(bfd, (stride * h) as i32, qh, ());
    let buf = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride as i32,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    frame.copy(&buf);
    if !wait_for(eq, app, 4, |a| a.scr_ready || a.scr_failed) || app.scr_failed {
        return None;
    }
    use std::io::{Read, Seek};
    let mut rf = std::fs::File::open(&path).unwrap();
    rf.seek(std::io::SeekFrom::Start((py * stride + px * 4) as u64))
        .unwrap();
    let mut px4 = [0u8; 4];
    rf.read_exact(&mut px4).unwrap();
    std::fs::remove_file(&path).ok();
    Some(px4)
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect to compositor");
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let _ = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    eq.roundtrip(&mut app).unwrap();

    if app.vptr.is_none() || app.fs_mgr.is_none() {
        println!("RESULT ok=false missing wp_viewporter/fractional_scale global");
        std::process::exit(0);
    }

    // ── Fullscreen two-tone toplevel ────────────────────────
    let surf = app.comp.clone().unwrap().create_surface(&qh, ());
    let xs = app.wm.clone().unwrap().get_xdg_surface(&surf, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    tl.set_title("vp-test".into());
    tl.set_fullscreen(None);
    surf.commit();
    if !wait_for(&mut eq, &mut app, 6, |a| a.cfg.is_some()) {
        println!("RESULT ok=false no fullscreen configure");
        std::process::exit(0);
    }
    let (w, h) = app.cfg.unwrap();
    let buf = two_tone(app.shm.as_ref().unwrap(), &qh, w, h, GREEN, BLUE);
    surf.attach(Some(&buf), 0, 0);
    surf.commit();
    // One presented frame so the compositor definitely holds
    // the new buffer before the first capture.
    app.frame_done = false;
    let _cb = surf.frame(&qh, ());
    surf.commit();
    wait_for(&mut eq, &mut app, 4, |a| a.frame_done);

    // Probe two spots — well inside each half, away from the
    // exact split so nearest-neighbor rounding can't flip them.
    let (lx, rx, cy) = (w / 4, 3 * w / 4, h / 2);

    // ── 1. baseline — left green, right blue ────────────────
    let l0 = capture_px(&mut eq, &mut app, &qh, lx, cy);
    let r0 = capture_px(&mut eq, &mut app, &qh, rx, cy);
    println!("VP-BASE l={l0:?} r={r0:?}");
    let base_ok = l0 == Some(GREEN) && r0 == Some(BLUE);

    // ── 2. viewport src=left half, dst=full → all green ─────
    let vp = app.vptr.clone().unwrap().get_viewport(&surf, &qh, ());
    // Source rect in surface-local units (the generator maps
    // fixed ↔ f64): left half stretched over the whole surface.
    vp.set_source(0.0, 0.0, (w / 2) as f64, h as f64);
    vp.set_destination(w as i32, h as i32);
    app.frame_done = false;
    let _cb = surf.frame(&qh, ());
    surf.commit();
    wait_for(&mut eq, &mut app, 4, |a| a.frame_done);
    let l1 = capture_px(&mut eq, &mut app, &qh, lx, cy);
    let r1 = capture_px(&mut eq, &mut app, &qh, rx, cy);
    println!("VP-SCALED l={l1:?} r={r1:?}");
    let scaled_ok = l1 == Some(GREEN) && r1 == Some(GREEN);

    // ── 3. unset — back to raw buffer ───────────────────────
    vp.set_source(-1.0, -1.0, -1.0, -1.0);
    vp.set_destination(-1, -1);
    app.frame_done = false;
    let _cb = surf.frame(&qh, ());
    surf.commit();
    wait_for(&mut eq, &mut app, 4, |a| a.frame_done);
    let l2 = capture_px(&mut eq, &mut app, &qh, lx, cy);
    let r2 = capture_px(&mut eq, &mut app, &qh, rx, cy);
    println!("VP-RESET l={l2:?} r={r2:?}");
    let reset_ok = l2 == Some(GREEN) && r2 == Some(BLUE);

    // ── 4. fractional scale — the manager must answer the
    // addon object with preferred_scale (×120). Every test
    // output runs scale 1.0 → exactly 120.
    let _fs = app
        .fs_mgr
        .clone()
        .unwrap()
        .get_fractional_scale(&surf, &qh, ());
    wait_for(&mut eq, &mut app, 4, |a| a.fs_scale.is_some());
    let fs_ok = app.fs_scale == Some(120);

    let ok = base_ok && scaled_ok && reset_ok && fs_ok;
    println!("RESULT ok={ok} base={base_ok} scaled={scaled_ok} reset={reset_ok} fs={fs_ok}");
}
