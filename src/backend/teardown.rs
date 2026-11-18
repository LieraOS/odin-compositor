// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Ordered session teardown — runs after the event loop stops.
//!
//! Struct drops alone leave two production-critical holes:
//! `std::process::Child::drop` never kills, so Xwayland would be
//! orphaned inside the logind session scope; and any wedged
//! blocking call inside the drop cascade (GPU `vkDeviceWaitIdle`
//! on a fence that never signals, a seat socket write against a
//! stuck daemon) pins the process — and with it the session
//! leader the display manager is waiting on before it restarts
//! the greeter. Observed in the field: a 50-minute hang after a
//! clean `quit` keybind.
//!
//! The watchdog therefore bounds the whole teardown: if the grace
//! period expires the process hard-exits via `_exit`, which closes
//! every fd instantly — DRM master released, seat device fds
//! closed, client sockets dead — and lets the session end.

use std::time::Duration;
use tracing::{info, warn};

/// How long the drop cascade may take before the process is
/// force-exited. Normal teardown is millisecond-scale; ten seconds
/// is generous for a wedged driver while still short enough for a
/// prompt greeter restart.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

/// Shut the compositor down in an order that leaves the seat, the
/// console and the client sockets usable for the next session:
///
/// 1. Arm the teardown watchdog — every step below plus the
///    remaining struct-drop cascade in `main` runs under it.
/// 2. Terminate Xwayland — `Child::drop` does not kill.
/// 3. Close the listening socket, then the display — every client
///    gets a real disconnect instead of a silently dead socket.
/// 4. `seat.disable()` releases the session's device access
///    promptly instead of waiting on fd-close side effects.
///
/// What is deliberately NOT done: the session scope's leftover
/// user processes (taskbar, terminals, dbus services) are reaped
/// by systemd once the session ends — killing user applications
/// is the scope's job, not the compositor's.
pub fn shutdown(state: &mut super::eventloop::CompState) {
    let _watchdog = std::thread::spawn(|| {
        std::thread::sleep(TEARDOWN_GRACE);
        warn!("teardown exceeded {:?} — forcing exit", TEARDOWN_GRACE);
        unsafe { libc::_exit(0) };
    });

    if let Some((xwm, _wm_fd)) = state.xwm.take() {
        let mut xwm = xwm;
        xwm.kill_child();
    }

    drop(state.listening.take());
    drop(state.comp.display.take());
    state.session.release();
    info!("teardown: xwayland stopped, clients closed, seat released");
}
