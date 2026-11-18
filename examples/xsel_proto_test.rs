// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Real X11 selection owner for the X→Wayland clipboard test.
//!
//! Connects to DISPLAY (Xwayland inside odin), takes the
//! CLIPBOARD selection and serves a fixed payload for the
//! text/UTF8 targets — the exact path a real X app's copy
//! takes. Runs until killed; the compositor-side proof is a
//! Wayland client seeing the offer and pasting the bytes.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_FROM_PARENT;

fn atom(conn: &impl Connection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes())
        .unwrap()
        .reply()
        .unwrap()
        .atom
}

fn main() {
    let (conn, screen) = x11rb::connect(None).expect("X connect failed");
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &x11rb::protocol::xproto::CreateWindowAux::new(),
    )
    .unwrap();
    let clipboard = atom(&conn, "CLIPBOARD");
    let targets = atom(&conn, "TARGETS");
    let utf8 = atom(&conn, "UTF8_STRING");
    let timestamp = atom(&conn, "TIMESTAMP");
    let payload: &[u8] = b"odin-x-clipboard-payload";

    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)
        .unwrap();
    conn.flush().unwrap();
    println!("XSEL-OWNER window={win}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();

    loop {
        let ev = match conn.wait_for_event() {
            Ok(e) => e,
            Err(_) => break,
        };
        if let Event::SelectionRequest(e) = ev {
            println!(
                "XSEL-REQ target={} prop={} req={}",
                e.target, e.property, e.requestor
            );
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            // TARGETS rides as a real 32-bit ATOM list; the byte
            // payloads go through change_property8 with their type.
            let atoms: Vec<u32> = vec![targets, timestamp, utf8, atom(&conn, "text/plain")];
            let (is_atom_list, value, ty) = if e.target == targets {
                (true, Vec::new(), u32::from(AtomEnum::ATOM))
            } else if e.target == utf8 || e.target == atom(&conn, "text/plain") {
                (false, payload.to_vec(), utf8)
            } else if e.target == timestamp {
                (
                    false,
                    (e.time as u32).to_ne_bytes().to_vec(),
                    u32::from(AtomEnum::INTEGER),
                )
            } else {
                (false, Vec::new(), 0)
            };
            let prop = if !is_atom_list && value.is_empty() {
                0 // refuse — SelectionNotify with property=None
            } else {
                let p = if e.property == 0 {
                    atom(&conn, "XSEL_DATA")
                } else {
                    e.property
                };
                if is_atom_list {
                    conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        p,
                        AtomEnum::ATOM,
                        &atoms,
                    )
                    .unwrap();
                } else {
                    conn.change_property8(PropMode::REPLACE, e.requestor, p, ty, &value)
                        .unwrap();
                }
                p
            };
            let notify = x11rb::protocol::xproto::SelectionNotifyEvent {
                response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property: prop,
            };
            conn.send_event(
                false,
                e.requestor,
                x11rb::protocol::xproto::EventMask::NO_EVENT,
                notify,
            )
            .unwrap();
            conn.flush().unwrap();
        }
        if let Event::SelectionClear(_) = ev {
            println!("XSEL-CLEARED");
            break;
        }
    }
}
