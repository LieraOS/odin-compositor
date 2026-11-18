// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X11 selection handling — the Wayland→X clipboard bridge.
//!
//! When a Wayland client sets the selection we own the X
//! CLIPBOARD selection through wm_win. X clients then send
//! SelectionRequest: TARGETS is answered from the source's mime
//! list, data targets start a wl_data_source.send into a pipe
//! whose read end waits in `pending` until EOF — then the bytes
//! land in the requestor's property and a SelectionNotify goes
//! out. Transfers pump once per tick so a slow writer never
//! blocks the event loop.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::Instant;
use tracing::info;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, SELECTION_NOTIFY_EVENT,
};
use x11rb::wrapper::ConnectionExt as _;

use super::Xwm;

use crate::compositor::CompState;

/// A clipboard transfer waiting on the Wayland source client —
/// send() hands it our pipe's write end; EOF completes the data.
pub struct PendingXfer {
    pub requestor: Window,
    pub selection: Atom,
    pub target: Atom,
    pub property: Atom,
    pub time: u32,
    pub read_fd: OwnedFd,
    pub buf: Vec<u8>,
    pub since: Instant,
}

/// Answer an X SelectionRequest — TARGETS mirrors the source's
/// mime list into atoms; data targets spawn a wl_data_source
/// send into a pipe queued as a PendingXfer.
pub fn request(xwm: &mut Xwm, comp: &mut CompState, e: &SelectionRequestEvent) {
    // ICCCM: property may be NONE for obsolete clients — write to
    // the target atom instead (the conventional fallback).
    let property = if e.property == 0 {
        e.target
    } else {
        e.property
    };
    let Some(src) = comp.selection.clone().filter(|s| s.is_alive()) else {
        return refuse(xwm, e);
    };
    let mimes = src.mimes();
    if e.target == xwm.atoms.targets {
        // TARGETS asks what we can serve — every advertised mime
        // becomes an atom; UTF8_STRING rides along for text.
        let mut targs = vec![xwm.atoms.targets, xwm.atoms.utf8_string];
        for m in &mimes {
            let a = intern(xwm, m);
            if !targs.contains(&a) {
                targs.push(a);
            }
        }
        let ok = xwm
            .conn
            .change_property32(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::ATOM,
                &targs,
            )
            .is_ok();
        xwm.conn.flush().ok();
        return notify(xwm, e, if ok { property } else { 0 });
    }
    // Target → mime mapping: utf8/STRING get the text fallbacks,
    // anything else resolves through its atom name.
    let wanted: Vec<&str> = if e.target == xwm.atoms.utf8_string {
        vec!["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"]
    } else if e.target == u32::from(AtomEnum::STRING) {
        vec!["text/plain", "text/plain;charset=utf-8"]
    } else {
        Vec::new()
    };
    let mime = wanted
        .iter()
        .find(|m| mimes.iter().any(|s| s == *m))
        .map(|s| s.to_string())
        .or_else(|| mimes.first().cloned());
    let Some(mime) = mime else {
        return refuse(xwm, e);
    };
    // send() hands the source client our pipe's write end — the
    // data arrives asynchronously while we keep servicing events.
    let Ok((rd, wr)) = rustix::pipe::pipe() else {
        return refuse(xwm, e);
    };
    unsafe { libc::fcntl(rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    src.send(mime, wr.as_fd());
    comp.flush_clients();
    drop(wr);
    xwm.pending.push(PendingXfer {
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property,
        time: e.time,
        read_fd: rd,
        buf: Vec::new(),
        since: Instant::now(),
    });
}

/// Advance pending transfers — nonblocking reads until the
/// source closes its end (EOF completes the data), then the
/// property write + SelectionNotify go out. Runs once per tick
/// via the backend's apply_requests path.
pub fn pump(xwm: &mut Xwm) {
    let mut done = Vec::new();
    for (i, p) in xwm.pending.iter_mut().enumerate() {
        let mut chunk = [0u8; 65536];
        loop {
            match rustix::io::read(&p.read_fd, &mut chunk) {
                Ok(0) => {
                    done.push(i);
                    break;
                }
                Ok(n) => p.buf.extend_from_slice(&chunk[..n]),
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    // Slow writer — give it 5s before sending a
                    // partial answer rather than hanging forever.
                    if p.since.elapsed() > std::time::Duration::from_secs(5) {
                        done.push(i);
                    }
                    break;
                }
                Err(_) => {
                    done.push(i);
                    break;
                }
            }
        }
    }
    // Complete in reverse index order so removals stay valid.
    for i in done.into_iter().rev() {
        let p = xwm.pending.remove(i);
        let ok = xwm
            .conn
            .change_property8(PropMode::REPLACE, p.requestor, p.property, p.target, &p.buf)
            .is_ok();
        let ev = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: p.time,
            requestor: p.requestor,
            selection: p.selection,
            target: p.target,
            property: if ok { p.property } else { 0 },
        };
        let _ = xwm
            .conn
            .send_event(false, p.requestor, EventMask::NO_EVENT, ev);
        xwm.conn.flush().ok();
        info!(
            requestor = p.requestor,
            bytes = p.buf.len(),
            "xwm: selection transfer done"
        );
    }
}

/// SelectionNotify — the ICCCM answer to every SelectionRequest.
fn notify(xwm: &Xwm, e: &SelectionRequestEvent, property: Atom) {
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: e.time,
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property,
    };
    let _ = xwm
        .conn
        .send_event(false, e.requestor, EventMask::NO_EVENT, ev);
    let _ = xwm.conn.flush();
}

/// Refusal — property NONE signals "conversion not available".
fn refuse(xwm: &Xwm, e: &SelectionRequestEvent) {
    notify(xwm, e, 0);
}

fn intern(xwm: &Xwm, name: &str) -> Atom {
    xwm.conn
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok().map(|r| r.atom))
        .unwrap_or(0)
}
