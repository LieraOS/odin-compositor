// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `xdg_system_bell_v1` — terminal bell requests from clients.
//! The bell is routed to the controlling tty (BEL byte): on the
//! DRM/session backend the kernel console driver turns it into
//! an audible beep where hardware supports it; nested sessions
//! simply have no tty and the write is skipped.

use wayland_protocols::xdg::system_bell::v1::server::{
    xdg_system_bell_v1 as sb, xdg_system_bell_v1::XdgSystemBellV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle};

use super::CompState;

pub use wayland_protocols::xdg::system_bell::v1::server::xdg_system_bell_v1::XdgSystemBellV1 as XdgSystemBell;

impl Dispatch<XdgSystemBellV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgSystemBellV1,
        request: sb::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            sb::Request::Ring { .. } => ring_bell(),
            sb::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Emit one BEL to the controlling terminal — the only bell path
/// a Wayland session still owns. Opening fresh each ring keeps
/// no fd and tolerates backends without a tty entirely.
fn ring_bell() {
    use std::io::Write;
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(b"\x07");
    }
}
