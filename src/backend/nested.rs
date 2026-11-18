// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Nested backend — odin running as a wayland client inside
//! a host session, for iteration without VT takeover.
//!
//! Own code on wayland-client (a libc-layer binding): connects to
//! the host compositor, creates one xdg_toplevel window, presents
//! shm frames, and pumps seat input back into the log. This is the
//! same development workflow the old build used, now fully owned.

use anyhow::{Context, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use tracing::info;
use wayland_client::protocol::{wl_compositor, wl_seat, wl_shm};
use wayland_client::Connection;
use wayland_protocols::xdg::shell::client::xdg_wm_base;

/// Globals the nested window needs — bound from the registry.
pub(crate) struct Nested {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub shm: Option<wl_shm::WlShm>,
    pub seat: Option<wl_seat::WlSeat>,
    pub xdg: Option<xdg_wm_base::XdgWmBase>,
    /// Set once the first configure arrives (window is live).
    pub configured: bool,
    /// Size the host asked for in xdg_toplevel.configure —
    /// (0,0) means "client picks". Honoring it is required:
    /// attaching an oversized buffer gets clipped to the
    /// assigned tile by the host's scene.
    pub win_size: (u32, u32),
    /// A configure changed win_size — the frame loop rebuilds
    /// the memfd buffer and the virtual output on the next tick.
    pub resize_pending: bool,
    /// Input event counters for the log.
    pub ptr_events: u64,
    pub key_events: u64,
    /// The compositor server — host input forwards into it.
    pub comp: Option<crate::compositor::CompState>,
}

/// Run the nested window: connect, map, present one gradient frame,
/// then dispatch input events forever. The frame data is written
/// through an mmap of the memfd.
pub fn run() -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to host compositor")?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<Nested>();
    let qh = queue.handle();
    let _reg = display.get_registry(&qh, ());
    let mut state = Nested {
        compositor: None,
        shm: None,
        seat: None,
        xdg: None,
        configured: false,
        win_size: (0, 0),
        resize_pending: false,
        ptr_events: 0,
        key_events: 0,
        comp: None,
    };
    queue.roundtrip(&mut state)?;

    let compositor = state.compositor.clone().context("no wl_compositor")?;
    let shm = state.shm.clone().context("no wl_shm")?;
    let xdg = state.xdg.clone().context("no xdg_wm_base")?;
    if let Some(seat) = &state.seat {
        seat.get_pointer(&qh, ());
        seat.get_keyboard(&qh, ());
    }

    let surface = compositor.create_surface(&qh, ());
    let xsurface = xdg.get_xdg_surface(&surface, &qh, ());
    let toplevel = xsurface.get_toplevel(&qh, ());
    toplevel.set_title("Odin (nested)".into());
    toplevel.set_app_id("odin".into());
    surface.commit();
    while !state.configured {
        queue.blocking_dispatch(&mut state)?;
    }
    info!("nested window mapped — presenting frame");

    // The real compositor server runs inside this backend — same
    // code the udev path will drive.
    let (mut comp_state, listening) = crate::compositor::CompState::new()?;
    // One virtual output covering the window — tiling, placement
    // and pointer clamping all see a real output rect. Size comes
    // from the host's first configure; 1280×800 is only the
    // fallback for hosts that leave sizing to the client (0×0).
    let (mut w, mut h) = if state.win_size.0 > 0 {
        state.win_size
    } else {
        (1280u32, 800u32)
    };
    comp_state.layout.add(
        "NESTED-1".to_string(),
        (w, h, 60),
        (0, 0),
        crate::wm::Edid::default(),
        vec![crate::wm::ModeSpec {
            w,
            h,
            hz: 60,
            preferred: true,
        }],
        true,
    );
    comp_state.space = (w as f64, h as f64);
    comp_state.register_output_globals();
    comp_state.ptr_pos = (w as f64 / 2.0, h as f64 / 2.0);
    // XWayland in nested mode too — same code path, same socket.
    let xwm = comp_state
        .display
        .as_mut()
        .and_then(|d| crate::xwm::spawn(d, std::sync::Arc::new(crate::compositor::CData::spawn())))
        .map(|(p, fd)| {
            comp_state.x_display = Some(p.display);
            comp_state.xwl_client = Some(p.client.clone());
            (crate::xwm::XwmState::Pending(p), fd)
        });
    let x_display = comp_state.x_display;
    state.comp = Some(comp_state);
    // Autostart — same contract as the real session: config-dir
    // lines + -S commands spawn onto our socket. Skipped during the
    // scripted selftest so user apps can't pollute the scene.
    if std::env::var("ODIN_NESTED_TEST").is_err() {
        let extra: Vec<String> = {
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
        crate::spawn::run_autostart(&extra, x_display);
    }
    let mut xwm = xwm;
    info!(
        socket = crate::spawn::socket_name(),
        "compositor server live — clients may connect"
    );
    let mut frame = crate::backend::nested_frame::make_frame(&shm, &qh, w, h)?;
    surface.attach(Some(&frame.buf), 0, 0);
    surface.commit();
    conn.flush()?;
    info!("nested running: animated frame + input dispatch");

    // Frame-paced loop: pump wayland events without blocking (so
    // frames keep flowing), paint+commit every ~16 ms.
    let mut tick: u64 = 0;
    loop {
        // Pump pending events first — dispatch_pending never blocks.
        loop {
            match queue.prepare_read() {
                Some(guard) => {
                    // Wait ≤16 ms for events — the same slice paces frames.
                    // Poll host-connection AND our compositor socket.
                    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
                    // Copy out RawFds — holding the guard's borrow
                    // across read() (which consumes it) is invalid.
                    let conn_raw = guard.connection_fd().as_raw_fd();
                    let srv_raw = listening.as_fd().as_raw_fd();
                    // The display backend's poll fd aggregates EVERY
                    // client socket — without it client requests
                    // only dispatch when something else wakes the
                    // poll (Xwayland's registry fetch hung forever).
                    let disp_raw = {
                        let c = state.comp.as_mut().unwrap();
                        c.display
                            .as_mut()
                            .map(|d| d.backend().poll_fd().as_raw_fd())
                            .unwrap_or(-1)
                    };
                    let xwm_raw = xwm
                        .as_ref()
                        .map(|(_, fd)| fd.as_fd().as_raw_fd())
                        .unwrap_or(-1);
                    let mut fds = vec![
                        PollFd::new(
                            unsafe { BorrowedFd::borrow_raw(conn_raw) },
                            PollFlags::POLLIN,
                        ),
                        PollFd::new(
                            unsafe { BorrowedFd::borrow_raw(srv_raw) },
                            PollFlags::POLLIN,
                        ),
                    ];
                    // Index bookkeeping — optional fds join after
                    // the fixed host+listen pair.
                    if disp_raw >= 0 {
                        fds.push(PollFd::new(
                            unsafe { BorrowedFd::borrow_raw(disp_raw) },
                            PollFlags::POLLIN,
                        ));
                    }
                    let xwm_idx = if xwm_raw >= 0 {
                        fds.push(PollFd::new(
                            unsafe { BorrowedFd::borrow_raw(xwm_raw) },
                            PollFlags::POLLIN,
                        ));
                        Some(fds.len() - 1)
                    } else {
                        None
                    };
                    let n = poll(&mut fds, PollTimeout::from(16u16))?;
                    if n > 0 {
                        if fds[0].revents().unwrap().contains(PollFlags::POLLIN) {
                            guard.read()?;
                        }
                        // X protocol events on the wm socket —
                        // handshake first, events once Ready.
                        if xwm_idx
                            .map(|i| fds[i].revents().unwrap().contains(PollFlags::POLLIN))
                            .unwrap_or(false)
                        {
                            if let Some((xstate, _)) = xwm.as_mut() {
                                match xstate {
                                    crate::xwm::XwmState::Pending(p) => {
                                        if let Some(x) = crate::xwm::drive_handshake(p) {
                                            *xstate = crate::xwm::XwmState::Ready(x);
                                        } else if p.dead {
                                            xwm = None;
                                        }
                                    }
                                    crate::xwm::XwmState::Ready(x) => {
                                        if crate::xwm::events::drain(
                                            x,
                                            state.comp.as_mut().unwrap(),
                                        )
                                        .is_err()
                                        {
                                            xwm = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Client accepts + request dispatch run on EVERY
                    // poll turn. The display poll fd proved
                    // unreliable — requests once sat unread for
                    // seconds — and empty pumps are cheap, so this
                    // does not gate on POLLIN or n>0.
                    {
                        let c = state.comp.as_mut().unwrap();
                        crate::compositor::pump(c, &listening)?;
                    };
                    break;
                }
                None => {
                    queue.dispatch_pending(&mut state)?;
                }
            }
        }
        queue.dispatch_pending(&mut state)?;
        tick += 1;
        // Host resized the window — rebuild the memfd frame at the
        // new extent and point the virtual output at it. Attaching
        // a buffer larger than the configured size is what clipped
        // the nested window to a corner of its host-assigned tile.
        if state.resize_pending {
            state.resize_pending = false;
            let (nw, nh) = state.win_size;
            if (nw, nh) != (w, h) && nw > 0 && nh > 0 {
                frame.buf.destroy();
                frame = crate::backend::nested_frame::make_frame(&shm, &qh, nw, nh)?;
                (w, h) = (nw, nh);
                let comp = state.comp.as_mut().unwrap();
                if let Some(out) = comp.layout.outputs.get_mut("NESTED-1") {
                    out.mode = (w, h, 60);
                }
                comp.layout.relayout();
                comp.space = comp.layout.bounds();
                comp.ptr_pos.0 = comp.ptr_pos.0.clamp(0.0, comp.space.0 - 1.0);
                comp.ptr_pos.1 = comp.ptr_pos.1.clamp(0.0, comp.space.1 - 1.0);
                comp.damage_everything();
                comp.dirty = true;
                comp.apply_layout();
                info!(w, h, "nested output resized to host configure");
            }
        }
        // quit binding → leave via the normal return path.
        if state.comp.as_ref().is_some_and(|c| c.quit) {
            break;
        }
        // Key repeat runs off the frame tick — same pacing the
        // udev backend's timerfd gives it.
        state.comp.as_mut().unwrap().repeat_tick();
        // Idle notifications + session-lock liveness on the
        // nested tick as well — same semantics as the udev loop.
        let c = state.comp.as_mut().unwrap();
        c.idle_tick();
        c.lock_tick();
        c.ws_sync();
        // Output-management ops have no hardware to act on in
        // nested — the layout change already landed in commit();
        // drain so they never pile up.
        c.pending_outops.clear();
        // Dead-client sweep on the tick — same contract as the
        // udev loop: ghost surfaces must not outlive clients.
        c.prune_dead();
        // Drive the XWM handshake every tick — the setup request
        // can't wait for POLLIN since the reply needs it first.
        let mut xwm_dead = false;
        if let Some((crate::xwm::XwmState::Pending(p), _)) = xwm.as_mut() {
            if let Some(x) = crate::xwm::drive_handshake(p) {
                if let Some((xs, _)) = xwm.as_mut() {
                    *xs = crate::xwm::XwmState::Ready(x);
                }
            } else if p.dead {
                xwm_dead = true;
            }
        }
        if xwm_dead {
            xwm = None; // dead handshake — stop polling the corpse
        }
        // Compositor→X requests (focus mirror, move push-back)
        // drain once per tick — they must not wait for X events.
        if let Some((crate::xwm::XwmState::Ready(x), _)) = xwm.as_mut() {
            crate::xwm::events::apply_requests(x, state.comp.as_mut().unwrap());
        }
        // Drain damage first — the backdrop repaint AND the
        // composite clip to the same rects (retained rendering).
        let dmg = {
            let comp = state.comp.as_mut().unwrap();
            comp.damage_taken = false;
            comp.present_damage()
        };
        // zwlr_output_power_manager_v1 — a powered-down nested
        // output stops compositing and presenting, exactly like
        // the CRTC-blanked DRM path. Frame callbacks stop too:
        // there is no vblank on a dead output.
        let nested_on = state
            .comp
            .as_ref()
            .map(|c| c.powered("NESTED-1"))
            .unwrap_or(true);
        if nested_on {
            frame.paint(tick, dmg.as_deref());
            // Composite client surfaces over the backdrop — the real
            // scene path, same call the udev renderer will make.
            {
                let px = unsafe { std::slice::from_raw_parts_mut(frame.ptr, (w * h * 4) as usize) };
                let comp = state.comp.as_mut().unwrap();
                comp.composite(px, w, h, 0, 0, true, dmg.as_deref());
                // Gamma LUT applies at present, like a CRTC ramp —
                // compositing and screencopy stay pre-gamma.
                if let Some(ramp) = state.comp.as_ref().unwrap().gamma_lut("NESTED-1") {
                    crate::compositor::gamma::apply_gamma(px, w, &ramp, dmg.as_deref());
                }
                state.comp.as_mut().unwrap().frame_done("NESTED-1");
            }
            // ODIN_NESTED_TEST — scripted run: two terminals, tiling
            // toggle, frame dump, exit. Pixel-checkable without a user.
            if std::env::var("ODIN_NESTED_TEST").is_ok() {
                crate::backend::nested_selftest::tick(tick, &mut state.comp, frame.ptr, w, h);
            }
            surface.attach(Some(&frame.buf), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
        }
        conn.flush()?;
        // Push outbound wayland traffic to our clients.
        if let Some(d) = state.comp.as_mut().and_then(|c| c.display.as_mut()) {
            let _ = d.flush_clients();
        }
    }
    Ok(())
}
