// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! The compositor's heartbeat — one calloop loop carrying every
//! backend source: session events (VT switches), libinput, DRM
//! vblank/page-flip completions, and the frame schedule timer.
//!
//! Frame pacing model (matches every production compositor):
//! page-flip completion or a fraction-of-interval timer marks the
//! output dirty → the render pass runs → the next flip is queued.
//! Input events are coalesced per dispatch batch and evaluated once
//! per loop turn, never per raw event — the 1000 Hz lesson.

use anyhow::Result;
use calloop::generic::Generic;
use calloop::{EventLoop, LoopSignal};
use calloop::{Interest, Mode, PostAction};
use std::os::fd::AsFd;
use tracing::info;
use wayland_server::ListeningSocket;

/// Shared compositor state the loop handlers touch. Grows as the
/// scene/output model lands.
pub struct CompState {
    pub running: bool,
    /// Frame pacing counter — consumed by the render schedule.
    #[allow(dead_code)]
    pub ticks: u64,
    /// The session stays inside the loop-owned state so the seat-fd
    /// source can dispatch into it.
    pub session: crate::session::Session,
    pub input: input::Libinput,
    /// The compositor server — same instance the nested backend
    /// uses, driven from real fds here. Owns the window/output
    /// layout including the floating↔tiling policy.
    pub comp: crate::compositor::CompState,
    /// Listening socket for wayland clients.
    #[allow(dead_code)]
    pub listening: Option<ListeningSocket>,
    /// Render outputs: one per connected connector — card + crtc
    /// + mode + scanout buffer, prepared by main before the loop.
    pub outputs: Vec<super::render::Output>,
    /// Timerfd kept for drain reads inside the frame source.
    pub frame_tfd: Option<std::os::fd::OwnedFd>,
    /// XWayland: the XWM state (Pending handshake → Ready) + a
    /// pollable dup of the wm socket fd.
    pub xwm: Option<(crate::xwm::XwmState, std::os::fd::OwnedFd)>,
    /// udev monitor on the drm subsystem — its socket goes
    /// readable on connector hotplug; the fd lives in a calloop
    /// source, this keeps the socket alive for `iter()`.
    pub udev_mon: Option<udev::MonitorSocket>,
    /// Set by the udev source when a drm change event arrived —
    /// the next frame tick re-scans connectors.
    pub hotplug: bool,
    /// inotify on the config DIRECTORY (editors replace the file
    /// atomically — a file watch dies on rename) so bindings.conf
    /// edits reload live; the fd lives in a calloop source, this
    /// keeps the Inotify alive for `read_events`.
    pub cfg_inot: Option<nix::sys::inotify::Inotify>,
    /// Set by the inotify source when bindings.conf changed —
    /// the next frame tick reloads and re-applies the config.
    pub cfg_dirty: bool,
}

/// Build the loop with its sources attached. Returning the
/// LoopSignal lets `run` arm a clean shutdown on Ctrl+C/SIGTERM.
pub fn build(state: &mut CompState) -> Result<(EventLoop<'static, CompState>, LoopSignal)> {
    let event_loop: EventLoop<'static, CompState> = EventLoop::try_new()?;
    let signal = event_loop.get_signal();

    // Wayland listening socket — accepts new clients on readable.
    if let Some(sock) = &state.listening {
        let sock_fd = rustix::io::dup(sock.as_fd())?;
        event_loop.handle().insert_source(
            Generic::new(sock_fd, Interest::READ, Mode::Level),
            |_, _, state| {
                if let Some(sock) = &state.listening {
                    let t = std::time::Instant::now();
                    let _ = crate::compositor::pump(&mut state.comp, sock);
                    state.comp.stat_pump_ms = state
                        .comp
                        .stat_pump_ms
                        .max(t.elapsed().as_secs_f64() * 1000.0);
                }
                Ok(PostAction::Continue)
            },
        )?;
    }

    // Wayland client traffic — the display's backend poll fd goes
    // readable whenever ANY connected client has pending requests;
    // without this source clients sit at get_registry forever.
    if let Some(display) = state.comp.display.as_mut() {
        let wl_fd = rustix::io::dup(display.backend().poll_fd())?;
        event_loop.handle().insert_source(
            Generic::new(wl_fd, Interest::READ, Mode::Level),
            |_, _, state| {
                if let Some(sock) = &state.listening {
                    let t = std::time::Instant::now();
                    let _ = crate::compositor::pump(&mut state.comp, sock);
                    state.comp.stat_pump_ms = state
                        .comp
                        .stat_pump_ms
                        .max(t.elapsed().as_secs_f64() * 1000.0);
                }
                Ok(PostAction::Continue)
            },
        )?;
    }

    // Frame timer ~60Hz: composite the scene and page-flip it to
    // the real CRTC — the udev equivalent of the nested present.
    let tfd = rustix::time::timerfd_create(
        rustix::time::TimerfdClockId::Realtime,
        rustix::time::TimerfdFlags::NONBLOCK | rustix::time::TimerfdFlags::CLOEXEC,
    )?;
    rustix::time::timerfd_settime(
        &tfd,
        rustix::time::TimerfdTimerFlags::empty(),
        &rustix::time::Itimerspec {
            it_interval: rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 16_666_666,
            },
            it_value: rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 16_666_666,
            },
        },
    )?;
    // Keep a duplicate for the drain read inside the callback.
    state.frame_tfd = Some(rustix::io::dup(&tfd)?);
    event_loop.handle().insert_source(
        Generic::new(tfd, Interest::READ, Mode::Level),
        |_ready, _, state| {
            // Drain the timerfd expiration count.
            let mut buf = [0u8; 8];
            if let Some(fd) = &state.frame_tfd {
                let _ = rustix::io::read(fd, &mut buf);
            }
            state.ticks += 1;
            // Key repeat piggy-backs on the frame tick — at 60Hz
            // the 40ms repeat interval lands within one tick.
            state.comp.repeat_tick();
            // Coalesced pointer dispatch — at most one motion
            // (and one accumulated relative-motion) per frame;
            // raw libinput rates would flood client repaints.
            state.comp.flush_motion();
            // Dead-client sweep every tick — a client that dies
            // between input events must not leave its surfaces
            // rendered as ghosts on the scene.
            state.comp.prune_dead();
            // Idle notifications + session-lock liveness — the
            // tick fires even when the scene is completely still.
            state.comp.idle_tick();
            state.comp.lock_tick();
            state.comp.ws_sync();
            // Fenced attaches: stashed buffers whose acquire
            // fence (explicit syncobj point or implicit dmabuf
            // fence) signaled since the last tick land on-screen.
            state.comp.sync_tick();
            // The XWM handshake also ticks here — the first write
            // can't wait for POLLIN since the reply only comes
            // after we send the setup request.
            let mut xwm_dead = false;
            if let Some((crate::xwm::XwmState::Pending(p), _)) = state.xwm.as_mut() {
                if let Some(x) = crate::xwm::drive_handshake(p) {
                    if let Some((xs, _)) = state.xwm.as_mut() {
                        *xs = crate::xwm::XwmState::Ready(x);
                    }
                } else if p.dead {
                    xwm_dead = true;
                }
            }
            if xwm_dead {
                // Xwayland's wm socket died — drop state + source
                // instead of polling a corpse every tick.
                state.xwm = None;
            }
            // quit binding asked for a clean exit — leave the
            // loop through the normal path (drops, logs).
            if state.comp.quit {
                state.running = false;
            }
            // Compositor→X requests (focus mirror, move push-back,
            // raises) drain per tick — independent of X event flow.
            if let Some((crate::xwm::XwmState::Ready(x), _)) = state.xwm.as_mut() {
                let t = std::time::Instant::now();
                crate::xwm::events::apply_requests(x, &mut state.comp);
                state.comp.stat_xwm_ms = state
                    .comp
                    .stat_xwm_ms
                    .max(t.elapsed().as_secs_f64() * 1000.0);
            }
            // Monitor hotplug — udev marked the flag; re-scan and
            // reconcile layout + DRM outputs before this frame.
            if state.hotplug {
                state.hotplug = false;
                super::hotplug::rescan(state);
            }
            // Config reload — bindings.conf changed under us
            // (settings UI, hand edit). Reload the whole file:
            // policies re-apply onto the live layout, bindings
            // take effect immediately, the scene retiles in place.
            if state.cfg_dirty {
                state.cfg_dirty = false;
                state.comp.cfg = crate::config::load();
                state.comp.apply_cfg();
                state.comp.apply_layout();
                state.comp.damage_everything();
            }
            // Validated output-configuration ops (re-modeset,
            // viewport move, CRTC blank) reach the hardware here.
            super::outcfg::apply_outops(state);
            // Damage-driven frames: composite only when the scene
            // changed — a fixed-cadence full-CPU composite would
            // starve input dispatch (libinput "event processing
            // lagging" errors).
            // Outputs that haven't presented yet still need their
            // frames — the master/EACCES retry lives inside frame().
            // Disabled/powered-off outputs don't count: they are
            // blanked once, then must not keep the loop compositing.
            let sweep_start = std::time::Instant::now();
            let needs_modeset = state.outputs.iter().any(|o| {
                !o.presented()
                    && state
                        .comp
                        .layout
                        .outputs
                        .get(&o.name)
                        .map(|x| x.enabled)
                        .unwrap_or(true)
                    && state.comp.powered(&o.name)
            });
            if state.comp.dirty || needs_modeset {
                state.comp.dirty = false;
                state.comp.damage_taken = false;
                for out in state.outputs.iter_mut() {
                    out.frame(&mut state.comp, &mut state.input);
                    // Modesetting permanently denied — exit cleanly
                    // so the display manager's greeter comes back.
                    if out.fatal {
                        state.running = false;
                    }
                    // Interleaved input drain after EVERY output —
                    // with several outputs each render pass can
                    // take tens of ms, and waiting for all of them
                    // leaves pointer motion queued in the kernel
                    // for the whole sum. One drain per output
                    // bounds the wait to a single render.
                    if let Err(err) = super::input::drain(&mut state.input, &mut state.comp) {
                        tracing::warn!(?err, "mid-tick input drain failed");
                    }
                }
                for out in state.outputs.iter_mut() {
                    out.frame_present(&mut state.comp, &mut state.input);
                    out.sync_cursor(&mut state.comp);
                }
            }
            // Settle pass: pipelined backends (Vulkan) hold their
            // newest frame GPU-side until its fence retires —
            // present it here so the last committed frame always
            // lands, even when no further damage arrives.
            for out in state.outputs.iter_mut() {
                out.settle(&mut state.comp, &mut state.input);
                if out.fatal {
                    state.running = false;
                }
            }
            let sweep_ms = sweep_start.elapsed().as_secs_f64() * 1000.0;
            state.comp.stat_frame_ms = state.comp.stat_frame_ms.max(sweep_ms);
            // Cursor-plane sync — image changes arriving via
            // commits re-upload here even with no pointer event.
            for out in state.outputs.iter_mut() {
                out.sync_cursor(&mut state.comp);
            }
            state.comp.sw_cursor = state.outputs.iter().any(|o| !o.hwcursor.active);
            // Frame-done callbacks and any other outbound events
            // queued during the frame leave the buffer now.
            state.comp.flush_clients();
            Ok(PostAction::Continue)
        },
    )?;
    let handle = event_loop.handle();

    // libinput fd — drains a batch per wake; events are coalesced
    // per dispatch call, never handled per raw event. The dup'd fd
    // shares the same file description, so readiness is identical.
    let input_fd = rustix::io::dup(state.input.as_fd())?;
    handle.insert_source(
        Generic::new(input_fd, Interest::READ, Mode::Level),
        |_event, _meta, state| {
            let t = std::time::Instant::now();
            if let Err(err) = super::input::drain(&mut state.input, &mut state.comp) {
                tracing::warn!(?err, "input drain failed");
            }
            state.comp.stat_drain_ms = state
                .comp
                .stat_drain_ms
                .max(t.elapsed().as_secs_f64() * 1000.0);
            // Hardware cursor — the move lands as a bare ioctl
            // with zero render-pipeline involvement, so pointer
            // latency never waits on a frame.
            for out in state.outputs.iter_mut() {
                out.sync_cursor(&mut state.comp);
            }
            state.comp.sw_cursor = state.outputs.iter().any(|o| !o.hwcursor.active);
            // Pointer/keyboard events queued during drain must reach
            // clients immediately — not at the next request dispatch.
            state.comp.flush_clients();
            Ok(PostAction::Continue)
        },
    )?;

    // XWM socket — handshake bytes first, X protocol events once
    // Ready (maps, configures, surface associations → comp.xwins).
    if let Some((_, fd)) = &state.xwm {
        let x_fd = rustix::io::dup(fd)?;
        event_loop.handle().insert_source(
            Generic::new(x_fd, Interest::READ, Mode::Level),
            |_, _, state| {
                let Some((xstate, _)) = state.xwm.as_mut() else {
                    return Ok(PostAction::Remove);
                };
                match xstate {
                    crate::xwm::XwmState::Pending(p) => {
                        if let Some(x) = crate::xwm::drive_handshake(p) {
                            *xstate = crate::xwm::XwmState::Ready(x);
                        } else if p.dead {
                            state.xwm = None;
                            return Ok(PostAction::Remove);
                        }
                    }
                    crate::xwm::XwmState::Ready(x) => {
                        let t = std::time::Instant::now();
                        let r = crate::xwm::events::drain(x, &mut state.comp);
                        state.comp.stat_xwm_ms = state
                            .comp
                            .stat_xwm_ms
                            .max(t.elapsed().as_secs_f64() * 1000.0);
                        if r.is_err() {
                            // Xwayland died — drop the source so we
                            // stop polling a dead socket.
                            state.xwm = None;
                            return Ok(PostAction::Remove);
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )?;
    }

    // udev monitor on the drm subsystem — connector plug/unplug
    // raises the hotplug flag the frame tick consumes. Without
    // this source the compositor only ever sees the boot-time
    // monitor set.
    match udev::MonitorBuilder::new()
        .and_then(|b| b.match_subsystem("drm"))
        .and_then(|b| b.listen())
    {
        Ok(mon) => {
            use std::os::fd::{AsRawFd, FromRawFd};
            let raw = mon.as_raw_fd();
            let dup = unsafe { libc::dup(raw) };
            if dup < 0 {
                tracing::warn!("udev monitor fd dup failed — hotplug disabled");
            } else {
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
                state.udev_mon = Some(mon);
                handle.insert_source(
                    Generic::new(fd, Interest::READ, Mode::Level),
                    |_, _, state| {
                        if let Some(mon) = &state.udev_mon {
                            // Drain the queue — coalesce any burst
                            // into a single rescan flag.
                            for _ in mon.iter() {}
                        }
                        state.hotplug = true;
                        Ok(PostAction::Continue)
                    },
                )?;
            }
        }
        Err(err) => {
            tracing::warn!(?err, "udev drm monitor unavailable — hotplug disabled");
        }
    }

    // inotify on the config dir — CLOSE_WRITE covers direct
    // writes, MOVED_TO covers the atomic-replace pattern every
    // editor uses. Settings apps get live reload for free: write
    // the file, the compositor re-applies on the next tick.
    {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        let cfg_dir = crate::config::path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).and_then(|i| {
            i.add_watch(
                &cfg_dir,
                AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MOVED_TO,
            )
            .map(|_| i)
        }) {
            Ok(inot) => {
                use std::os::fd::{AsFd, AsRawFd, FromRawFd};
                let dup = unsafe { libc::dup(inot.as_fd().as_raw_fd()) };
                if dup < 0 {
                    tracing::warn!("inotify fd dup failed — config reload disabled");
                } else {
                    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
                    state.cfg_inot = Some(inot);
                    handle.insert_source(
                        Generic::new(fd, Interest::READ, Mode::Level),
                        |_, _, state| {
                            if let Some(inot) = &state.cfg_inot {
                                if let Ok(events) = inot.read_events() {
                                    for ev in events {
                                        // Only our file counts — other
                                        // writes in the dir are noise.
                                        if ev.name.map(|n| n == "bindings.conf").unwrap_or(false) {
                                            state.cfg_dirty = true;
                                        }
                                    }
                                }
                            }
                            Ok(PostAction::Continue)
                        },
                    )?;
                }
            }
            Err(err) => {
                tracing::warn!(?err, "config dir watch unavailable — reload disabled");
            }
        }
    }

    // seat fd — VT switches and enable/disable arrive here.
    let seat_fd = rustix::io::dup(state.session.fd()?)?;
    handle.insert_source(
        Generic::new(seat_fd, Interest::READ, Mode::Level),
        |_event, _meta, state| {
            let _ = state.session.dispatch(0);
            Ok(PostAction::Continue)
        },
    )?;
    Ok((event_loop, signal))
}

/// Run the loop until SIGTERM/SIGINT stops it.
#[allow(dead_code)]
pub fn run(event_loop: &mut EventLoop<'static, CompState>, state: &mut CompState) -> Result<()> {
    // SIGTERM/SIGINT → set the stop flag; dispatch wakes via the
    // loop's own wake source when needed.
    unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, || {
            // Note: async-signal-safe flag flip only.
        })
        .ok();
    }
    info!("event loop running");
    while state.running {
        event_loop.dispatch(None, state)?;
    }
    Ok(())
}
