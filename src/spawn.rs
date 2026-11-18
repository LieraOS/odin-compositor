// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client spawning — keybinding "spawn" actions and session
//! autostart. Every spawned process gets WAYLAND_DISPLAY plus
//! DISPLAY when Xwayland is up, so X11 tools in an autostart
//! line work identically to native Wayland ones.

use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::info;

/// The compositor socket name this process actually bound —
/// set once at bind time, read by every client spawn so
/// spawned processes always reach THIS instance even when a
/// second compositor (nested dev run) took odin-0 first.
static SOCKET_NAME: OnceLock<String> = OnceLock::new();

/// Record the bound socket name — called once by the compositor
/// socket setup after `ListeningSocket::bind` succeeds.
pub(crate) fn set_socket_name(name: &str) {
    let _ = SOCKET_NAME.set(name.to_string());
}

/// The socket name clients should connect to — `odin-0` until
/// the real bind is recorded.
pub(crate) fn socket_name() -> &'static str {
    SOCKET_NAME.get().map(|s| s.as_str()).unwrap_or("odin-0")
}

/// Execute a spawn binding — "spawn X" launches a client on our
/// socket. `display` is the X display number X apps get via
/// DISPLAY (None → Xwayland not running). Non-spawn actions are
/// handled by the seat dispatch itself (they mutate compositor
/// state — e.g. quit sets CompState::quit for a clean exit).
pub(crate) fn run_binding(action: &str, display: Option<u32>) {
    if let Some(cmd) = action.strip_prefix("spawn ") {
        info!(cmd, "binding: spawn");
        spawn_cmd(cmd, display);
    }
}

/// Spawn one shell command with the session environment —
/// shared by keybinding spawns and the autostart file.
pub(crate) fn spawn_cmd(cmd: &str, display: Option<u32>) {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(cmd).env("WAYLAND_DISPLAY", socket_name());
    if let Some(d) = display {
        c.env("DISPLAY", format!(":{d}"));
    }
    c.spawn().ok();
}

/// The autostart file: `~/.config/odin/autostart` — one shell
/// command per line, `#` comments allowed. Compositor-generic:
/// a session script picks its desktop by what's listed here.
pub fn autostart_cmds() -> Vec<String> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    let path = base.join("odin/autostart");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    info!(?path, "autostart file loaded");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Run all autostart commands — called once the compositor
/// socket is listening so every spawned client connects cleanly.
/// `extra` carries -S/--session commands from the command line.
pub fn run_autostart(extra: &[String], display: Option<u32>) {
    for cmd in autostart_cmds().iter().chain(extra.iter()) {
        info!(cmd, "autostart: spawn");
        spawn_cmd(cmd, display);
    }
}
