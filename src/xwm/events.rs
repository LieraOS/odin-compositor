// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! XWM event handling — X11 protocol events → compositor state.
//!
//! The redirect means X windows only ever *request* changes; we
//! decide what actually happens. MapRequest creates the xwins
//! entry and performs the real map, WL_SURFACE_ID client messages
//! bind X windows to their wl_surface, and geometry flows from
//! ConfigureRequest into SurfaceData.pos so the scene renderer
//! places X windows like any other.

use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, EventMask, StackMode,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

use wayland_server::Resource;

use super::assoc::{surface_of, try_bind};
use super::{XReq, Xwm};
use crate::compositor::{CompState, SurfaceData, Xwin};

/// Drain all pending X events — called when the wm socket fd is
/// readable. Returns Err on connection loss (Xwayland exited).
pub fn drain(xwm: &mut Xwm, comp: &mut CompState) -> anyhow::Result<()> {
    // Retry pending associations first — a WL_SURFACE_ID message
    // can reach us before Xwayland's wl_surface create has been
    // pumped through the display's client fd.
    let pending: Vec<u32> = comp
        .xwins
        .iter()
        .filter(|(_, xw)| xw.surface_id.is_some() && !xw.associated)
        .map(|(w, _)| *w)
        .collect();
    for wid in pending {
        try_bind(comp, wid);
    }
    let mut seen = 0u32;
    loop {
        let ev = match xwm.conn.poll_for_event() {
            Ok(Some(e)) => {
                seen += 1;
                e
            }
            Ok(None) => break,
            Err(e) => {
                warn!(?e, seen, "xwm: connection error — Xwayland gone");
                return Err(e.into());
            }
        };
        match ev {
            // Override-redirect windows never ask the WM — they
            // surface via CreateNotify/MapNotify (X menus, tooltips).
            Event::CreateNotify(e) => {
                if e.parent != xwm.root {
                    continue;
                }
                comp.xwins.insert(
                    e.window,
                    Xwin {
                        x: e.x as i32,
                        y: e.y as i32,
                        w: e.width as u32,
                        h: e.height as u32,
                        surface_id: None,
                        associated: false,
                        mapped: false,
                        fullscreen: false,
                        maximized: false,
                        saved_geo: None,
                        override_redirect: e.override_redirect,
                    },
                );
            }
            Event::MapNotify(e) => {
                // Copy out what the remap check needs — the
                // mutable xwins borrow must end before
                // surface_of borrows comp immutably.
                let (or, sid) = comp
                    .xwins
                    .get_mut(&e.window)
                    .map(|xw| {
                        xw.mapped = true;
                        (xw.override_redirect, xw.surface_id)
                    })
                    .unwrap_or((false, None));
                // An override-redirect window re-mapping after an
                // unmap must return to the floating list — X
                // clients don't necessarily commit a buffer on
                // remap, so waiting for a commit could leave it
                // invisible.
                if or {
                    if let Some(surf) = surface_of(comp, sid) {
                        comp.map_xpopup(&surf);
                    }
                }
                comp.damage_everything();
            }
            Event::MapRequest(e) => {
                if e.parent != xwm.root {
                    continue; // child windows map inside their parent
                }
                // Read the window's current geometry — rootless
                // Xwayland positions windows itself.
                let geo = xwm
                    .conn
                    .get_geometry(e.window)
                    .ok()
                    .and_then(|c| c.reply().ok());
                let (x, y, w, h) = geo
                    .map(|g| (g.x as i32, g.y as i32, g.width as u32, g.height as u32))
                    .unwrap_or((0, 0, 640, 480));
                // Subscribe to the window's own events so unmaps,
                // destroys and property changes reach us.
                xwm.conn
                    .change_window_attributes(
                        e.window,
                        &ChangeWindowAttributesAux::new()
                            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                    )
                    .ok();
                comp.xwins.insert(
                    e.window,
                    Xwin {
                        x,
                        y,
                        w,
                        h,
                        surface_id: None,
                        associated: false,
                        mapped: true,
                        fullscreen: false,
                        maximized: false,
                        saved_geo: None,
                        override_redirect: false,
                    },
                );
                // Actually perform the map — the redirect only
                // intercepted the request.
                xwm.conn.map_window(e.window).ok();
                // EWMH: the WM owns _NET_WM_STATE — write the
                // initial (empty) list so clients see a managed
                // window from the start.
                comp.xreqs.push(super::XReq::SetState {
                    win: e.window,
                    fullscreen: false,
                    maximized: false,
                    minimized: false,
                });
                xwm.conn.flush().ok();
                info!(win = e.window, x, y, w, h, "xwm: window mapped");
            }
            Event::UnmapNotify(e) => {
                let sid = comp
                    .xwins
                    .get_mut(&e.window)
                    .map(|xw| {
                        xw.mapped = false;
                        xw.surface_id
                    })
                    .flatten();
                if let Some(surf) = surface_of(comp, sid) {
                    // An unmapped X window must stop rendering —
                    // it keeps its wl_surface, so only its order
                    // slot leaves; a re-map re-joins on commit.
                    comp.ft_closed(&surf);
                    comp.order.retain(|s| s.id() != surf.id());
                    comp.xpopup_remove(&surf);
                    comp.damage_everything();
                }
                info!(win = e.window, "xwm: window unmapped");
            }
            Event::DestroyNotify(e) => {
                if let Some(xw) = comp.xwins.remove(&e.window) {
                    if let Some(surf) = surface_of(comp, xw.surface_id) {
                        comp.ft_closed(&surf);
                        comp.order.retain(|s| s.id() != surf.id());
                        comp.xpopup_remove(&surf);
                    }
                    comp.damage_everything();
                    info!(win = e.window, "xwm: window destroyed");
                }
            }
            Event::ConfigureNotify(e) => {
                // The applied geometry — for override-redirect
                // windows this is the ONLY position source (they
                // configure themselves; nothing reaches
                // ConfigureRequest). Our own ConfigureWindow
                // calls echo here too, keeping managed windows
                // in sync either way.
                let (pos, sid) = {
                    let Some(xw) = comp.xwins.get_mut(&e.window) else {
                        continue;
                    };
                    xw.x = e.x as i32;
                    xw.y = e.y as i32;
                    xw.w = e.width as u32;
                    xw.h = e.height as u32;
                    ((xw.x, xw.y), xw.surface_id)
                };
                if let Some(surf) = surface_of(comp, sid) {
                    if let Some(sd) = surf.data::<SurfaceData>() {
                        let mut p = sd.pos.lock().unwrap();
                        if *p != pos {
                            *p = pos;
                            comp.damage_everything();
                        }
                    }
                }
            }
            Event::ConfigureRequest(e) => {
                let mask = u16::from(e.value_mask);
                // Update the stored geometry first — the surface
                // sync below needs `comp` unborrowed.
                let (pos, sid) = {
                    let Some(xw) = comp.xwins.get_mut(&e.window) else {
                        xwm.conn.flush().ok();
                        continue;
                    };
                    if mask & 1 != 0 {
                        xw.x = e.x as i32;
                    }
                    if mask & 2 != 0 {
                        xw.y = e.y as i32;
                    }
                    if mask & 4 != 0 {
                        xw.w = e.width as u32;
                    }
                    if mask & 8 != 0 {
                        xw.h = e.height as u32;
                    }
                    ((xw.x, xw.y), xw.surface_id)
                };
                // Push the position onto the live surface — the
                // scene graph reads pos directly.
                if let Some(surf) = surface_of(comp, sid) {
                    if let Some(sd) = surf.data::<SurfaceData>() {
                        *sd.pos.lock().unwrap() = pos;
                    }
                }
                comp.damage_everything();
                // Ack the request — real WMs may alter it; we
                // accept and notify so clients know the result.
                // Only MASKED fields go back on the wire: an
                // unset x/y in the request carries 0, and echoing
                // it would teleport the window to the origin.
                let mut aux = ConfigureWindowAux::new();
                if mask & 1 != 0 {
                    aux = aux.x(e.x as i32);
                }
                if mask & 2 != 0 {
                    aux = aux.y(e.y as i32);
                }
                if mask & 4 != 0 {
                    aux = aux.width(e.width as u32);
                }
                if mask & 8 != 0 {
                    aux = aux.height(e.height as u32);
                }
                if u16::from(e.value_mask) & 16 != 0 {
                    aux = aux.sibling(e.sibling);
                }
                if u16::from(e.value_mask) & 32 != 0 {
                    aux = aux.stack_mode(StackMode::from(e.stack_mode));
                }
                xwm.conn.configure_window(e.window, &aux).ok();
                xwm.conn.flush().ok();
            }
            // X clients pasting from the Wayland clipboard —
            // answered through wm_win's selection ownership.
            Event::SelectionRequest(e) => {
                crate::xwm::selection::request(xwm, comp, &e);
            }
            // An X client took/released CLIPBOARD — the X→WL
            // direction tracks ownership through XFIXES.
            Event::XfixesSelectionNotify(e) => {
                crate::xwm::xsel::owner_changed(xwm, comp, &e);
            }
            // SelectionRequest answers addressed to our wm window
            // — TARGETS queries and paste fetches.
            Event::SelectionNotify(e) => {
                crate::xwm::xsel::selection_notify(xwm, comp, &e);
            }
            // Lost the CLIPBOARD selection to a real X client —
            // the X→Wayland direction will pick it up there.
            Event::SelectionClear(e) => {
                if e.selection == xwm.atoms.clipboard {
                    info!("xwm: X client took CLIPBOARD ownership");
                }
            }
            Event::ClientMessage(e) => {
                super::client_msg::handle(xwm, comp, e);
            }
            Event::Error(e) => {
                // X protocol errors would otherwise vanish — a
                // failed redirect or bad request shows up here.
                warn!(?e, "xwm: X protocol error");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Drain compositor-side requests onto the X connection —
/// focus mirroring, move push-backs, raises. Called once per
/// frame tick by the backend so comp.xreqs never goes stale.
pub fn apply_requests(xwm: &mut Xwm, comp: &mut CompState) {
    // Clipboard transfers pump every tick — their pipes fill
    // while the compositor keeps running, independent of xreqs.
    crate::xwm::selection::pump(xwm);
    // X→Wayland direction: paste requests queued by receive()
    // become SelectionRequests here.
    crate::xwm::xsel::fetch_queued(xwm, comp);
    let reqs = std::mem::take(&mut comp.xreqs);
    if reqs.is_empty() {
        return;
    }
    use x11rb::protocol::xproto::{AtomEnum, ConfigureWindowAux, InputFocus, PropMode, StackMode};
    for r in reqs {
        match r {
            XReq::Activate(w) => {
                if w != 0 {
                    xwm.conn
                        .set_input_focus(InputFocus::PARENT, w, x11rb::CURRENT_TIME)
                        .ok();
                }
                xwm.conn
                    .change_property32(
                        PropMode::REPLACE,
                        xwm.root,
                        xwm.atoms.net_active_window,
                        AtomEnum::WINDOW,
                        &[w],
                    )
                    .ok();
            }
            XReq::Configure { win, x, y, w, h } => {
                let mut aux = ConfigureWindowAux::new().x(x).y(y);
                if let Some(w) = w {
                    aux = aux.width(w);
                }
                if let Some(h) = h {
                    aux = aux.height(h);
                }
                xwm.conn.configure_window(win, &aux).ok();
            }
            XReq::SetState {
                win,
                fullscreen,
                maximized,
                minimized,
            } => {
                // _NET_WM_STATE mirrors the compositor's real
                // decision back into the X client's property.
                let mut list = Vec::new();
                if fullscreen {
                    list.push(xwm.atoms.net_wm_state_fullscreen);
                }
                if maximized {
                    list.push(xwm.atoms.net_wm_state_max_vert);
                    list.push(xwm.atoms.net_wm_state_max_horz);
                }
                if minimized {
                    list.push(xwm.atoms.net_wm_state_hidden);
                }
                xwm.conn
                    .change_property32(
                        PropMode::REPLACE,
                        win,
                        xwm.atoms.net_wm_state,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .ok();
            }
            XReq::Raise(w) => {
                xwm.conn
                    .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
                    .ok();
            }
            XReq::OwnClipboard => {
                // The Wayland selection changed — claim CLIPBOARD
                // so X clients' paste requests reach us.
                info!(wm_win = xwm.wm_win, "xwm: claiming CLIPBOARD");
                xwm.conn
                    .set_selection_owner(xwm.wm_win, xwm.atoms.clipboard, x11rb::CURRENT_TIME)
                    .ok();
            }
            XReq::ClearClipboard => {
                // Atom NONE — releases the selection to nobody.
                xwm.conn
                    .set_selection_owner(0u32, xwm.atoms.clipboard, x11rb::CURRENT_TIME)
                    .ok();
            }
        }
    }
    xwm.conn.flush().ok();
}
