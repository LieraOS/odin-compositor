// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Async X11 setup handshake — drives the wm-socket state
//! machine one step per event-loop wake until the connection
//! is ready, then turns it into a live `Xwm`.

use std::io::{Read, Write};
use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

use super::{client_message, intern, Atoms, Pending, Xwm};

/// Drive the async handshake one step — call whenever the wm fd
/// is readable or writable. Returns Some(Xwm) on completion.
pub fn drive_handshake(p: &mut Pending) -> Option<Xwm> {
    let stream = p.stream.as_mut()?;
    if !p.request_sent {
        // X11 setup request: byte-order 'l', protocol 11.0, no
        // auth — same bytes Connect::with_authorization produces.
        let req: &[u8] = &[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        match stream.write(req) {
            Ok(n) if n == req.len() => p.request_sent = true,
            Ok(_) => return None, // short write: retry whole thing
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => {
                warn!(?e, "xwm: handshake write failed — giving up");
                p.stream = None;
                p.dead = true;
                return None;
            }
        }
    }
    let adv = match stream.read(p.connect.as_mut()?.buffer()) {
        Ok(0) => {
            warn!("xwm: handshake eof — Xwayland closed wm socket");
            p.stream = None;
            p.dead = true;
            return None;
        }
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
        Err(e) => {
            warn!(?e, "xwm: handshake read failed — giving up");
            p.stream = None;
            p.dead = true;
            return None;
        }
    };
    if !p.connect.as_mut()?.advance(adv) {
        return None;
    }
    // Setup complete — ownership of the stream moves into
    // x11rb's DefaultStream.
    let stream = p.stream.take()?;
    let (dstream, _) = DefaultStream::from_unix_stream(stream).ok()?;
    let conn =
        RustConnection::for_connected_stream(dstream, p.connect.take()?.into_setup().ok()?).ok()?;
    let root = conn.setup().roots[0].root;
    let atoms = Atoms {
        wl_surface_id: intern(&conn, "WL_SURFACE_ID"),
        wm_change_state: intern(&conn, "WM_CHANGE_STATE"),
        net_wm_state: intern(&conn, "_NET_WM_STATE"),
        net_wm_name: intern(&conn, "_NET_WM_NAME"),
        wm_protocols: intern(&conn, "WM_PROTOCOLS"),
        wm_delete_window: intern(&conn, "WM_DELETE_WINDOW"),
        net_wm_window_type: intern(&conn, "_NET_WM_WINDOW_TYPE"),
        motif_wm_hints: intern(&conn, "_MOTIF_WM_HINTS"),
        net_active_window: intern(&conn, "_NET_ACTIVE_WINDOW"),
        net_moveresize: intern(&conn, "_NET_WM_MOVERESIZE"),
        clipboard: intern(&conn, "CLIPBOARD"),
        targets: intern(&conn, "TARGETS"),
        utf8_string: intern(&conn, "UTF8_STRING"),
        net_wm_state_fullscreen: intern(&conn, "_NET_WM_STATE_FULLSCREEN"),
        net_wm_state_max_vert: intern(&conn, "_NET_WM_STATE_MAXIMIZED_VERT"),
        net_wm_state_max_horz: intern(&conn, "_NET_WM_STATE_MAXIMIZED_HORZ"),
        net_wm_state_hidden: intern(&conn, "_NET_WM_STATE_HIDDEN"),
    };
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(
            EventMask::SUBSTRUCTURE_REDIRECT
                | EventMask::SUBSTRUCTURE_NOTIFY
                | EventMask::PROPERTY_CHANGE,
        ),
    )
    .ok();
    // ICCCM manager selection: Xwayland deliberately leaves its
    // listen sockets dormant under -wm until the WM claims WM_S0.
    // Own a tiny window and take the selection — X clients only
    // get accepted after this.
    let wm_s0 = intern(&conn, "WM_S0");
    let wm_win = conn.generate_id().unwrap_or(0);
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        wm_win,
        root,
        -1,
        -1,
        1,
        1,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &x11rb::protocol::xproto::CreateWindowAux::new().override_redirect(1),
    )
    .ok();
    conn.set_selection_owner(wm_win, wm_s0, x11rb::CURRENT_TIME)
        .ok();
    // EWMH basics — toolkits check _NET_SUPPORTED and
    // _NET_SUPPORTING_WM_CHECK before trusting a WM. The WM_S0
    // window doubles as the supporting-wm-check window.
    let net_supported = intern(&conn, "_NET_SUPPORTED");
    let net_wm_check = intern(&conn, "_NET_SUPPORTING_WM_CHECK");
    let utf8 = intern(&conn, "UTF8_STRING");
    use x11rb::protocol::xproto::{AtomEnum, PropMode};
    conn.change_property32(
        PropMode::REPLACE,
        wm_win,
        net_wm_check,
        AtomEnum::WINDOW,
        &[wm_win],
    )
    .ok();
    conn.change_property32(
        PropMode::REPLACE,
        root,
        net_wm_check,
        AtomEnum::WINDOW,
        &[wm_win],
    )
    .ok();
    conn.change_property8(PropMode::REPLACE, wm_win, atoms.net_wm_name, utf8, b"odin")
        .ok();
    conn.change_property32(
        PropMode::REPLACE,
        root,
        net_supported,
        AtomEnum::ATOM,
        &[
            atoms.net_wm_state,
            atoms.net_wm_name,
            atoms.wm_protocols,
            atoms.wm_delete_window,
            atoms.net_wm_window_type,
            atoms.motif_wm_hints,
            atoms.net_active_window,
            atoms.net_moveresize,
        ],
    )
    .ok();
    // Broadcast MANAGER so any waiting clients learn the WM is up.
    let manager = intern(&conn, "MANAGER");
    client_message(
        &conn,
        root,
        manager,
        [x11rb::CURRENT_TIME, wm_s0, wm_win, 0, 0],
    );
    // Composite redirect — rootless Xwayland only creates a
    // wl_surface for windows marked RedirectDrawManual. One call
    // on the root covers all present + future children.
    use x11rb::protocol::composite::{self, ConnectionExt as _};
    conn.composite_redirect_subwindows(root, composite::Redirect::MANUAL)
        .ok();
    // XFIXES SelectionNotify — the X→Wayland clipboard watches
    // CLIPBOARD ownership through this channel.
    super::xsel::init(&conn, wm_win, atoms.clipboard);
    info!(
        root,
        x_display = p.display,
        "xwm: redirect active — Xwayland WM up"
    );
    Some(Xwm {
        conn,
        child: p.child.take()?,
        root,
        atoms,
        lease: p.lease.take()?,
        wm_win,
        pending: Vec::new(),
        xsel_pending: Vec::new(),
    })
}
