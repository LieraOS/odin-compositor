// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Seat session control via libseat (seatd or logind backend —
//! libseat picks whichever is present).
//!
//! The compositor must own the session before it may open DRM
//! devices or VT-bound input devices; without it, ioctls fail
//! with EPERM or the GPU stays bound to a foreign master.

use anyhow::{Context, Result};
use libseat::{Device, Seat, SeatEvent};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// Active seat session — handle for device open/close.
///
/// Each device path is taken through seatd exactly once: the
/// seat-owned `Device` stays alive for the session's lifetime and
/// callers get a `dup()` of its fd. seatd rejects a second
/// `open_device` on the same path from the same client (EINVAL),
/// and dropping the Device between scan and output-init created
/// exactly that re-open — the correct seat-device discipline
/// is: open once, keep for the session.
pub struct Session {
    seat: Seat,
    devices: HashMap<PathBuf, Device>,
}

impl Session {
    /// Open the seat for the current user session. The callback
    /// receives enable/disable transitions (VT switches, pause).
    pub fn open() -> Result<Self> {
        let mut seat = Seat::open(move |_seat, event| match event {
            SeatEvent::Enable => info!("seat session enabled (master acquired)"),
            SeatEvent::Disable => info!("seat session disabled (master lost)"),
        })
        .context("libseat open_seat failed — seatd/logind missing?")?;
        // The enable_seat event can be queued right after connect —
        // drain it now so later synchronous calls see a clean wire.
        let _ = seat.dispatch(0);
        info!(backend = seat.name(), "seat session opened");
        Ok(Self {
            seat,
            devices: HashMap::new(),
        })
    }

    /// Open a session-managed device (e.g. /dev/dri/card0 or an
    /// input node). The seat takes the device once; callers receive
    /// a fresh `dup()`ed fd they own outright — the seat's original
    /// stays open and is revoked by libseat on session pause.
    pub fn open_device(&mut self, path: &Path) -> Result<OwnedFd> {
        if let Some(device) = self.devices.get(path) {
            return rustix::io::dup(device).with_context(|| format!("dup seat device {path:?}"));
        }
        let device = self
            .seat
            .open_device(&path)
            .with_context(|| format!("opening device {path:?}"))?;
        let fd = rustix::io::dup(&device).context("dup new seat device")?;
        self.devices.insert(path.to_owned(), device);
        debug!(?path, "device opened via seat");
        Ok(fd)
    }

    /// Open a seat bound to the session's foreground VT. seatd
    /// binds each client to whatever VT is active at `Seat::open`
    /// time — a client that opened while the greeter's VT was up
    /// gets DISABLED the moment it switches the console itself
    /// (observed: seatd sends Disable, then refuses open_device
    /// with EPERM). The only sound order is therefore:
    ///   1. wait out the display manager's own asynchronous chvt
    ///      (LightDM bug #63 — session scripts work around it by
    ///      polling the very same sysfs node),
    ///   2. if it never comes, ask seatd to switch for us, then
    ///      CLOSE AND REOPEN the seat so the new client binds to
    ///      the now-foreground VT,
    ///   3. console ioctls as last resort for the tty-login path.
    pub fn open_on_vt() -> Result<Self> {
        let vt = session_vt();
        if let Some(v) = vt {
            if wait_fg_vt(v, 4000) {
                info!(vt = v, "display manager activated session VT");
            }
        }
        let mut session = Self::open()?;
        if let Some(v) = vt {
            if fg_vt() != format!("tty{v}") {
                match session.seat.switch_session(v as i32) {
                    Ok(()) => info!(vt = v, "seatd session switch requested"),
                    Err(err) => {
                        tracing::warn!(?err, "switch_session failed — trying console ioctls");
                        let _ = session.console_activate(v);
                    }
                }
                if wait_fg_vt(v, 3000) {
                    info!(vt = v, "session VT active — reopening seat on new VT");
                    drop(session);
                    session = Self::open()?;
                } else {
                    tracing::warn!(vt = v, fg = fg_vt(), "VT switch did not complete");
                }
            }
        }
        Ok(session)
    }

    /// Classic path: open our own VT device, claim it as the
    /// controlling tty, take VT process-control mode, activate.
    fn console_activate(&self, vt: u32) -> Result<()> {
        use std::os::fd::AsRawFd;
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/tty{vt}"))
            .with_context(|| format!("open /dev/tty{vt}"))?;
        let fd = tty.as_raw_fd();
        const TIOCSCTTY: libc::c_ulong = 0x540E;
        const VT_SETMODE: libc::c_ulong = 0x5602;
        const VT_ACTIVATE: libc::c_ulong = 0x5606;
        const VT_WAITACTIVE: libc::c_ulong = 0x5607;
        unsafe {
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
            libc::signal(libc::SIGUSR2, libc::SIG_IGN);
            libc::ioctl(fd, TIOCSCTTY as libc::Ioctl, 0);
        }
        // struct vt_mode {char mode, waitv; short relsig, acqsig,
        // frsig} — 8 bytes. mode=VT_PROCESS(1): kernel routes VT
        // switch requests through us; relsig/acqsig are the ignored
        // SIGUSR1/2 (LE shorts), frsig unused.
        let mode: [u8; 8] = [1, 0, 10, 0, 12, 0, 0, 0];
        unsafe {
            libc::ioctl(
                fd,
                VT_SETMODE as libc::Ioctl,
                mode.as_ptr() as *const libc::c_void,
            )
        };
        if unsafe { libc::ioctl(fd, VT_ACTIVATE as libc::Ioctl, vt as libc::c_int) } < 0 {
            return Err(std::io::Error::last_os_error()).context("VT_ACTIVATE failed");
        }
        if unsafe { libc::ioctl(fd, VT_WAITACTIVE as libc::Ioctl, vt as libc::c_int) } < 0 {
            return Err(std::io::Error::last_os_error()).context("VT_WAITACTIVE failed");
        }
        Ok(())
    }

    /// The fd to register on the main loop — seat events arrive
    /// through it (VT switch, enable/disable).
    pub fn fd(&mut self) -> Result<std::os::fd::BorrowedFd<'_>> {
        self.seat.get_fd().context("libseat get_fd")
    }

    /// Dispatch pending seat events (enable/disable transitions).
    pub fn dispatch(&mut self, timeout_ms: i32) -> Result<i32> {
        self.seat
            .dispatch(timeout_ms)
            .context("libseat dispatch failed")
    }

    /// Release the seat explicitly at shutdown — tells seatd/logind
    /// the session is done with its devices instead of leaving the
    /// release to fd-close side effects deep in the drop cascade.
    /// The remaining handle teardown (close) still happens via Drop.
    pub fn release(&mut self) {
        if let Err(err) = self.seat.disable() {
            tracing::warn!(?err, "seat disable on shutdown failed");
        }
    }
}

/// This session's assigned VT: XDG_VTNR under a display manager,
/// else the controlling tty's minor for the tty-login path.
fn session_vt() -> Option<u32> {
    std::env::var("XDG_VTNR")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .or_else(ctty_vt)
}

/// VT number encoded as the minor of our controlling tty — dev_t
/// in /proc/self/stat field 7; /dev/ttyN has major 4, minor N.
fn ctty_vt() -> Option<u32> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = stat.rsplit(')').next()?;
    // fields[0]=state,[1]=ppid,[2]=pgrp,[3]=session,[4]=tty_nr
    let tty_nr: u64 = tail.split_whitespace().nth(4)?.parse().ok()?;
    if tty_nr == 0 {
        return None;
    }
    let major = ((tty_nr >> 8) & 0xfff) | ((tty_nr >> 32) & !0xfff);
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xffff_ff00);
    (major == 4 && minor > 0).then_some(minor as u32)
}

/// Name of the foreground console, e.g. "tty8" — world-readable
/// sysfs view of `VT_GETSTATE`, works without any device access.
fn fg_vt() -> String {
    std::fs::read_to_string("/sys/class/tty/tty0/active")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Poll the foreground VT for `timeout_ms` — display managers and
/// seatd perform the kernel switch asynchronously, and touching
/// DRM before it lands leaves us without master.
fn wait_fg_vt(vt: u32, timeout_ms: u64) -> bool {
    let want = format!("tty{vt}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if fg_vt() == want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fg_vt() == want
}
