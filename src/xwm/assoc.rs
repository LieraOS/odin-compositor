// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X↔wl_surface association — WL_SURFACE_ID binding, the
//! client-scoped surface lookup and the delayed-bind retry.

use tracing::info;
use wayland_server::Resource;

use super::Xwm;
use crate::compositor::{CompState, SurfaceData, Xwin};

/// The wl_surface matching an Xwayland surface id. Protocol ids
/// are client-local — an id like 16 can exist in EVERY client's
/// namespace, so matching must be restricted to surfaces owned
/// by the Xwayland client (comp.xwl_client) or a same-numbered
/// surface from another client would swallow the association.
pub(crate) fn surface_of(
    comp: &CompState,
    sid: Option<u32>,
) -> Option<wayland_server::protocol::wl_surface::WlSurface> {
    let sid = sid?;
    let xclient = comp.xwl_client.as_ref()?;
    comp.all_surfaces
        .iter()
        .find(|s| {
            s.is_alive()
                && s.id().protocol_id() == sid
                && s.client().map(|c| c.id() == xclient.id()).unwrap_or(false)
        })
        .cloned()
}

/// Record the surface id an X window claims, then try the bind —
/// the wl_surface may already exist or may show up on a later
/// create_surface dispatch (the two channels race).
pub(crate) fn associate(_xwm: &mut Xwm, comp: &mut CompState, wid: u32, sid: u32) {
    let xw = comp.xwins.entry(wid).or_insert(Xwin {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        surface_id: None,
        associated: false,
        mapped: false,
        fullscreen: false,
        maximized: false,
        saved_geo: None,
        override_redirect: false,
    });
    xw.surface_id = Some(sid);
    try_bind(comp, wid);
    if comp.xwins.get(&wid).is_some_and(|x| !x.associated) {
        info!(
            win = wid,
            sid, "xwm: surface id pending — no wl_surface yet"
        );
    }
}

/// Resolve an X window's recorded surface id against the live
/// wl_surface list — called on WL_SURFACE_ID arrival, on X event
/// drain retries, and from create_surface when a new surface may
/// close a pending association.
pub(crate) fn try_bind(comp: &mut CompState, wid: u32) {
    let Some(xw) = comp.xwins.get(&wid) else {
        return;
    };
    if xw.associated {
        return;
    }
    let Some(sid) = xw.surface_id else { return };
    let pos = (xw.x, xw.y);
    let Some(surf) = surface_of(comp, Some(sid)) else {
        return;
    };
    let Some(sd) = surf.data::<SurfaceData>() else {
        return;
    };
    *sd.xwin.lock().unwrap() = Some(wid);
    if let Some(xw) = comp.xwins.get_mut(&wid) {
        xw.associated = true;
    }
    // Apply the X geometry the surface didn't know about — its
    // cascade position was guessed before association.
    *sd.pos.lock().unwrap() = pos;
    // A buffer may have been committed before this association —
    // without a window role it stayed out of the stacking order,
    // so map it now that the xwin link landed. Override-redirect
    // windows (menus, tooltips) float at their X11 geometry
    // instead of joining order/tiling.
    if sd.shown_buf.lock().unwrap().is_some() {
        let or = comp
            .xwins
            .get(&wid)
            .map(|x| x.override_redirect)
            .unwrap_or(false);
        if or {
            comp.map_xpopup(&surf);
        } else {
            comp.map_window(&surf);
        }
    }
    comp.damage_everything();
    info!(
        win = wid,
        sid,
        x = pos.0,
        y = pos.1,
        "xwm: surface associated"
    );
}
