// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X→Wayland clipboard — the inverse of `selection.rs`.
//!
//! XFIXES SelectionNotify tells us when an X client takes the
//! CLIPBOARD selection. We then fetch its TARGETS, wrap the
//! resulting mime list in a `SelSrc::X`, and publish it through
//! the normal selection machinery — Wayland clients see a
//! regular offer. Their `receive` queues an X fetch here:
//! convert_selection → SelectionNotify → get_property → the
//! bytes land in the paste client's pipe fd.

use std::os::fd::OwnedFd;

use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, SelectionNotifyEvent};
use x11rb::rust_connection::RustConnection;

use super::Xwm;
use crate::compositor::data::SelSrc;
use crate::compositor::CompState;

/// An in-flight SelectionRequest we sent on wm_win's behalf.
pub struct XFetch {
    /// The property the owner writes its answer into.
    prop: u32,
    kind: XFetchKind,
}

enum XFetchKind {
    /// Owner-change TARGETS query — the ATOM list reply becomes
    /// the offered mime list.
    Targets { owner: u32 },
    /// A paste fetch — the reply's bytes go to this pipe fd.
    Paste { fd: OwnedFd },
}

/// Selection-change subscription — call once the WM is up.
/// XFIXES 1.0 carries SelectionNotify; we ask for 4.0 so
/// XCLIENT_CLOSE events (owner died) reach us too.
pub fn init(conn: &RustConnection, wm_win: u32, clipboard: u32) {
    match conn.xfixes_query_version(4, 0).map(|c| c.reply()) {
        Ok(Ok(_)) => {
            conn.xfixes_select_selection_input(
                wm_win,
                clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .ok();
            conn.flush().ok();
            info!("xwm: XFIXES selection tracking armed");
        }
        _ => warn!("xwm: no XFIXES — X→Wayland clipboard unavailable"),
    }
}

/// An X client changed CLIPBOARD ownership. owner==wm_win is our
/// own claim (Wayland source), owner==0 means the X side let go.
pub fn owner_changed(xwm: &mut Xwm, comp: &mut CompState, e: &xfixes::SelectionNotifyEvent) {
    if e.selection != xwm.atoms.clipboard {
        return;
    }
    if e.owner == xwm.wm_win {
        return; // our own claim — Wayland owns, nothing to do
    }
    if e.owner == 0 {
        // The X owner released/died — drop a stale X-backed
        // selection so pastes stop being offered.
        if matches!(comp.selection, Some(SelSrc::X(_))) {
            info!("xwm: X clipboard released");
            comp.set_selection(None);
        }
        return;
    }
    // A real X client owns it now — fetch its TARGETS list and
    // build the Wayland-side source from the reply.
    info!(owner = e.owner, "xwm: X client owns CLIPBOARD");
    let prop = intern(xwm, "_ODIN_SEL_TARGETS");
    xwm.conn
        .convert_selection(
            xwm.wm_win,
            xwm.atoms.clipboard,
            xwm.atoms.targets,
            prop,
            e.timestamp,
        )
        .ok();
    xwm.xsel_pending.push(XFetch {
        prop,
        kind: XFetchKind::Targets { owner: e.owner },
    });
    xwm.conn.flush().ok();
}

/// A SelectionRequest answer addressed to wm_win — matches one
/// pending fetch by property.
pub fn selection_notify(xwm: &mut Xwm, comp: &mut CompState, e: &SelectionNotifyEvent) {
    info!(req = e.requestor, prop = e.property, "xsel: notify");
    if e.requestor != xwm.wm_win || e.property == 0 {
        return;
    }
    let Some(idx) = xwm.xsel_pending.iter().position(|f| f.prop == e.property) else {
        return;
    };
    let fetch = xwm.xsel_pending.remove(idx);
    let reply = match xwm
        .conn
        .get_property(true, xwm.wm_win, e.property, AtomEnum::ANY, 0, u32::MAX)
        .map(|c| c.reply())
    {
        Ok(Ok(r)) => r,
        other => {
            warn!(?other, "xsel: get_property failed");
            return;
        }
    };
    info!(
        bytes = reply.value.len(),
        ty = reply.type_,
        "xsel: property read"
    );
    match fetch.kind {
        XFetchKind::Targets { owner } => publish(xwm, comp, owner, &reply.value),
        XFetchKind::Paste { fd } => {
            // The X owner's payload — straight into the paste
            // client's pipe, same as a Wayland source.send would.
            let _ = rustix::io::write(&fd, &reply.value);
        }
    }
}

/// Compositor-queued paste fetches (wayland client called
/// receive on an X-backed offer) — turn each into a real
/// SelectionRequest.
pub fn fetch_queued(xwm: &mut Xwm, comp: &mut CompState) {
    let queued = std::mem::take(&mut comp.x_paste_q);
    for (owner, mime, fd) in queued {
        let target = mime_to_atom(xwm, &mime);
        let prop = intern(xwm, "_ODIN_SEL_DATA");
        xwm.conn
            .convert_selection(
                xwm.wm_win,
                xwm.atoms.clipboard,
                target,
                prop,
                x11rb::CURRENT_TIME,
            )
            .ok();
        xwm.xsel_pending.push(XFetch {
            prop,
            kind: XFetchKind::Paste { fd },
        });
        info!(owner, mime, "xwm: X→Wayland paste fetch started");
    }
    xwm.conn.flush().ok();
}

/// TARGETS reply → SelSrc::X → the normal selection machinery.
fn publish(xwm: &mut Xwm, comp: &mut CompState, owner: u32, value: &[u8]) {
    if value.len() % 4 != 0 {
        warn!(len = value.len(), "xsel: odd TARGETS reply");
        return;
    }
    let mut mimes = Vec::new();
    for chunk in value.chunks_exact(4) {
        let atom = u32::from_ne_bytes(chunk.try_into().unwrap());
        let Ok(name) = xwm.conn.get_atom_name(atom).map(|c| {
            c.reply()
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        }) else {
            continue;
        };
        let Ok(name) = name else { continue };
        // Selection-protocol atoms aren't pasteable mimes.
        if matches!(
            name.as_str(),
            "TARGETS" | "MULTIPLE" | "TIMESTAMP" | "SAVE_TARGETS" | "CLIPBOARD" | "PRIMARY"
        ) {
            continue;
        }
        mimes.push(name);
    }
    if mimes.is_empty() {
        warn!("xsel: TARGETS answer had no usable mimes");
        return;
    }
    info!(
        owner,
        mimes = mimes.len(),
        "xwm: X clipboard → wayland offer"
    );
    comp.set_selection(Some(SelSrc::X(std::sync::Arc::new(
        crate::compositor::data::XSelData {
            owner,
            mimes,
            dead: std::sync::atomic::AtomicBool::new(false),
        },
    ))));
}

/// Wayland mime → X target atom. The text family all maps onto
/// UTF8_STRING — X owners advertise that, not the wl name.
fn mime_to_atom(xwm: &Xwm, mime: &str) -> u32 {
    match mime {
        "UTF8_STRING" | "text/plain;charset=utf-8" | "text/plain" | "STRING" | "TEXT" => {
            xwm.atoms.utf8_string
        }
        other => intern(xwm, other),
    }
}

fn intern(xwm: &Xwm, name: &str) -> u32 {
    xwm.conn
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0)
}
