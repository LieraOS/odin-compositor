// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Session-environment publication — once the compositor's
//! socket and XWayland display exist, the D-Bus and systemd
//! activation environments must learn about them or every
//! dbus-activated service (portals, settings daemons, session
//! components like taskbars) fails or stalls on its
//! activation timeout. Other compositors publish the same way
//! the moment their socket goes live.

use std::process::Command;
use tracing::debug;

/// Session identity resolution — whatever the session layer set
/// before launching the compositor wins: a desktop environment
/// knows its own name, a compositor must not override it (it
/// breaks OnlyShowIn autostart filtering, portals and session
/// scripts). "odin" only fills the bare-compositor case where
/// the variable was never set.
fn desktop_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "odin".to_string())
}

/// Push WAYLAND_DISPLAY / DISPLAY / session identity into the
/// user's D-Bus and systemd activation environments. Errors are
/// deliberately swallowed: a session without a bus (tty-only
/// runs, nested backends) must still come up.
pub fn publish(socket: &str, x_display: Option<u32>) {
    let cur_desktop = desktop_env("XDG_CURRENT_DESKTOP");
    let ses_desktop = desktop_env("XDG_SESSION_DESKTOP");
    let mut vars = vec![
        format!("WAYLAND_DISPLAY={socket}"),
        "XDG_SESSION_TYPE=wayland".to_string(),
        format!("XDG_SESSION_DESKTOP={ses_desktop}"),
        format!("XDG_CURRENT_DESKTOP={cur_desktop}"),
    ];
    if let Some(d) = x_display {
        vars.push(format!("DISPLAY=:{d}"));
    }
    let status = Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(&vars)
        .status();
    debug!(?status, "dbus activation environment published");

    // Same export into the systemd user manager — services
    // started via `systemctl --user` (autostart units, portals)
    // read THIS environment, not the dbus one.
    let mut names = vec![
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_SESSION_DESKTOP",
        "XDG_CURRENT_DESKTOP",
    ];
    if x_display.is_some() {
        names.push("DISPLAY");
    }
    let status = Command::new("systemctl")
        .args(["--user", "import-environment"])
        .args(names)
        .env("WAYLAND_DISPLAY", socket)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_SESSION_DESKTOP", &ses_desktop)
        .env("XDG_CURRENT_DESKTOP", &cur_desktop)
        .env(
            "DISPLAY",
            x_display.map(|d| format!(":{d}")).unwrap_or_default(),
        )
        .status();
    debug!(?status, "systemd user environment published");
}
