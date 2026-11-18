// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! odin — a standalone Wayland compositor.
//!
//! Phase 1 (this milestone): session control, DRM connector scan,
//! output enumeration, libinput event loop. Everything above the
//! libc-layer bindings is own code — no compositor framework.

mod backend;
mod compositor;
mod config;
mod cursor;
mod input_conf;
mod log;
mod renderer;
mod session;
mod session_env;
mod spawn;
mod wm;
mod xcursor;
mod xwm;

use anyhow::{Context, Result};
use tracing::{info, warn};

fn main() {
    // Logging first — every path must log, including nested mode.
    let log_path = log::init();
    info!(log = %log_path.display(), "odin starting");
    // On a real VT stderr goes nowhere — every fatal error must
    // land in the persistent log, never die silently.
    if let Err(err) = run() {
        tracing::error!(?err, "odin fatal error");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // Debug probe: `odin --vk-probe /dev/dri/cardN` initializes the
    // vulkan device context against a DRM node and reports the
    // match — verifies physical-device selection on real hardware
    // without needing a session or a VT.
    if let Some(flag) = std::env::args()
        .nth(1)
        .as_deref()
        .filter(|a| *a == "--vk-probe")
    {
        let _ = flag;
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/dev/dri/card1".into());
        let fd = std::fs::File::open(&path).context("open drm node")?;
        use std::os::fd::AsRawFd;
        let ctx = renderer::vulkan::context_probe(fd.as_raw_fd())?;
        info!("vk-probe ok: {:?}", ctx);
        return Ok(());
    }

    // Full-pipeline test: `odin --vk-render-test /dev/dri/cardN`
    // uploads a texture, draws it through the real pipeline, reads
    // the frame back and asserts the pixels. Must pass on real
    // hardware before a vulkan build ships to a session.
    if std::env::args().nth(1).as_deref() == Some("--vk-render-test") {
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/dev/dri/card1".into());
        let fd = std::fs::File::open(&path).context("open drm node")?;
        use std::os::fd::AsRawFd;
        let res = renderer::vulkan::selftest::render_test(fd.as_raw_fd())?;
        info!("vk-render-test ok: {res}");
        return Ok(());
    }

    // First-submit latency probe: `odin --vk-submit-test` times
    // each stage of the new-surface → first-submit path on real
    // hardware — isolates driver-level deferred materialization
    // stalls without needing a session restart.
    if std::env::args().nth(1).as_deref() == Some("--vk-submit-test") {
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/dev/dri/card1".into());
        let fd = std::fs::File::open(&path).context("open drm node")?;
        use std::os::fd::AsRawFd;
        renderer::vulkan::submit_test::submit_test(fd.as_raw_fd())?;
        info!("vk-submit-test done");
        return Ok(());
    }

    // Full-frame probe: `odin --vk-render-probe` compares submit
    // latency between a warm draw and one that first references a
    // freshly arena-bound texture — the session's real workload.
    if std::env::args().nth(1).as_deref() == Some("--vk-render-probe") {
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/dev/dri/card1".into());
        let fd = std::fs::File::open(&path).context("open drm node")?;
        use std::os::fd::AsRawFd;
        renderer::vulkan::submit_test::render_probe(fd.as_raw_fd())?;
        info!("vk-render-probe done");
        return Ok(());
    }

    // DMA-upload probe: `odin --vk-dma-test` exercises the
    // production staging path (cached staging → graphics-queue
    // vkCmdCopyBufferToImage → OPTIMAL/VRAM texture) end to end
    // on real hardware — the historic Xid 31 trap was the
    // dedicated copy engine, which this path never touches.
    if std::env::args().nth(1).as_deref() == Some("--vk-dma-test") {
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/dev/dri/card1".into());
        let fd = std::fs::File::open(&path).context("open drm node")?;
        use std::os::fd::AsRawFd;
        renderer::vulkan::submit_test::dma_test(fd.as_raw_fd())?;
        info!("vk-dma-test done");
        return Ok(());
    }

    // Nested development mode: run as a wayland client inside the
    // running host session — no VT grab.
    if std::env::var("ODIN_BACKEND").as_deref() == Ok("nested") {
        return backend::nested::run();
    }

    // Session control: open the seat bound to this session's VT —
    // open_on_vt waits for the display manager's chvt, switches
    // via seatd itself when it never comes, and reopens the seat
    // so the client binds to the correct foreground VT. Required
    // before touching /dev/dri/card*.
    let mut session = session::Session::open_on_vt().context("opening seat session")?;

    // Discover GPU devices the session exposes and enumerate their
    // connectors — the ground truth for wl_output objects later.
    let scan = backend::drm::scan(&mut session).context("drm scan")?;
    backend::drm::report(&scan);

    // Output layout from the scan: every connected connector is an
    // output with its preferred mode as extent; the first is
    // primary until Settings says otherwise.
    let mut layout = wm::Layout::new();
    for c in scan.connectors.iter().filter(|c| c.connected) {
        layout.add(
            c.name.clone(),
            c.mode,
            c.physical_mm,
            c.edid.clone(),
            c.modes.clone(),
            layout.outputs.is_empty(),
        );
    }

    // Input must initialise while `session` is still local —
    // afterwards it moves into the loop-owned state.
    info!("init: libinput...");
    let input = backend::input::init(&mut session)
        .context("libinput init — without this the keyboard is dead")?;
    info!("init: libinput ok");

    // The compositor server runs on the udev path too — same code
    // the nested backend drives, here over real fds.
    info!("init: compositor server...");
    let (mut comp, listening) = compositor::CompState::new()?;
    info!("init: compositor server ok");

    // GPU modifier probe BEFORE Xwayland spawns — the dmabuf
    // global must advertise the real sampled-modifier list at
    // first bind, or XWayland's DRI3 path falls back to
    // software (pixman) rendering for every X client.
    for c in scan.connectors.iter().filter(|c| c.connected) {
        if let Ok(fd) = session.open_device(&c.card) {
            use std::os::fd::AsRawFd;
            let mods = renderer::vulkan::context::VkContext::probe_modifiers(fd.as_raw_fd());
            if !mods.is_empty() {
                info!(card = %c.card.display(), count = mods.len(), "dma modifiers probed");
                break;
            }
        }
    }

    // XWayland — Xwayland is a normal wayland client on our
    // socket plus an X11 window-manager peer we play over x11rb.
    // Missing X stack → None, the session runs wayland-only.
    let xwm = comp
        .display
        .as_mut()
        .and_then(|d| xwm::spawn(d, std::sync::Arc::new(compositor::CData::spawn())))
        .map(|(p, fd)| {
            comp.x_display = Some(p.display);
            comp.xwl_client = Some(p.client.clone());
            (xwm::XwmState::Pending(p), fd)
        });

    // Session identity into the dbus/systemd activation
    // environments — without it, dbus-activated clients
    // (portals, session components) can't find the display
    // and stall on activation timeouts.
    session_env::publish(spawn::socket_name(), comp.x_display);

    // Autostart — the compositor socket is listening now, so
    // session scripts (`odin -S <session-command>`) and
    // ~/.config/odin/autostart lines can connect immediately.
    let session_cmd: Vec<String> = {
        let mut args = std::env::args().skip(1);
        let mut v = Vec::new();
        while let Some(a) = args.next() {
            if a == "-S" || a == "--session" {
                if let Some(cmd) = args.next() {
                    v.push(cmd);
                }
            }
        }
        v
    };
    spawn::run_autostart(&session_cmd, comp.x_display);

    // The pointer may roam the union of all output rects — the
    // layout's bounding box, not a single monitor. The layout
    // itself moves into the compositor — window arrangement and
    // the layout policy live there.
    comp.space = layout.bounds();
    comp.layout = layout;
    // User's monitor choices: which connector is primary and
    // which per-output layout policy (floating/tiling) each
    // screen starts with. set_primary re-runs the auto-layout —
    // safe at boot, no user-arranged positions exist yet.
    if let Some(p) = comp.cfg.primary.clone() {
        comp.layout.set_primary(&p);
    }
    comp.apply_cfg();
    // Per-connector wl_output globals can only register now —
    // before the scan the layout had no names to advertise.
    comp.register_output_globals();

    // One render output per connected connector — each owns its
    // CRTC, dumb buffer and viewport slice of the global scene.
    let mut used_crtcs = std::collections::HashSet::new();
    let mut outputs = Vec::new();
    for c in scan.connectors.iter().filter(|c| c.connected) {
        let pos = comp
            .layout
            .outputs
            .get(&c.name)
            .map(|o| (o.x, o.y))
            .unwrap_or((0, 0));
        info!("init: output {}...", c.name);
        match backend::render::Output::new(&mut session, &c.card, &c.name, pos, &mut used_crtcs) {
            Ok(o) => outputs.push(o),
            Err(err) => warn!(conn = c.name, ?err, "output init failed — skipping"),
        }
    }
    if outputs.is_empty() {
        warn!("no connected output — running headless");
    }
    // Canonical syncobj device fd — the render node paired with
    // the first working card. Timeline imports mint their
    // handles in this fd's table (NVIDIA rejects fd_to_handle on
    // the client's exported syncobj fd). Render nodes are
    // user-openable; no seat fd needed. Nested leaves it None —
    // the protocol path lazily opens one on first import.
    if comp.drm_fd.is_none() {
        comp.drm_fd = scan
            .connectors
            .first()
            .and_then(|c| backend::drm::render_node_for(&c.card))
            .and_then(|p| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(p)
                    .ok()
            })
            .map(Into::into);
    }

    let mut state = backend::eventloop::CompState {
        running: true,
        ticks: 0,
        session,
        input,
        comp,
        listening: Some(listening),
        outputs,
        frame_tfd: None,
        xwm,
        udev_mon: None,
        hotplug: false,
        cfg_inot: None,
        cfg_dirty: false,
    };
    info!("init: eventloop build...");
    let (mut event_loop, _signal) = backend::eventloop::build(&mut state)?;
    info!("init: eventloop built — running");
    backend::eventloop::run(&mut event_loop, &mut state)?;

    info!("odin exiting cleanly");
    // Ordered teardown — kills Xwayland, closes clients, releases
    // the seat, and bounds the drop cascade with a watchdog so a
    // wedged driver call can never pin the session leader.
    backend::teardown::shutdown(&mut state);
    drop(state);
    Ok(())
}
