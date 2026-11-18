// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X window state handling — `_NET_WM_STATE` applies and the
//! property mirror back to the X world.
//!
//! X clients ask for fullscreen/maximize with a `_NET_WM_STATE`
//! ClientMessage; the compositor decides (EWMH: the WM owns the
//! result) and mirrors the outcome into the `_NET_WM_STATE`
//! property. X windows have no xdg configure — a state apply
//! moves+resizes the real X window and points the paired
//! wl_surface at the new spot.

use tracing::info;

use wayland_server::Resource;

use super::XReq;
use crate::compositor::{CompState, SurfaceData};

/// The state a `_NET_WM_STATE` message can toggle — hidden is
/// minimized and lives on SurfaceData, not here.
#[derive(Clone, Copy)]
pub enum XState {
    Fullscreen,
    Maximized,
}

/// Apply or clear a window state on an X window: save the
/// pre-state geometry once, move+resize to the output rect on
/// set, restore on clear. `surf` may be missing (association
/// pending) — the X window still gets configured.
pub fn set_xwin_state(
    comp: &mut CompState,
    win: u32,
    kind: XState,
    on: bool,
    surf: Option<&wayland_server::protocol::wl_surface::WlSurface>,
) {
    // Resolve the output rect the state should cover — the
    // output holding the window's center, primary as fallback.
    let geo = comp.xwins.get(&win).map(|x| (x.x, x.y, x.w, x.h));
    let rect = geo
        .and_then(|(x, y, w, h)| {
            let (cx, cy) = (x + w as i32 / 2, y + h as i32 / 2);
            comp.layout
                .outputs
                .values()
                .find(|o| {
                    o.enabled
                        && cx >= o.x
                        && cx < o.x + o.mode.0 as i32
                        && cy >= o.y
                        && cy < o.y + o.mode.1 as i32
                })
                .map(|o| (o.x, o.y, o.mode.0, o.mode.1))
        })
        .map(|(x, y, w, h)| (x, y, w, h))
        .unwrap_or_else(|| comp.layout.primary_rect());
    let Some(xw) = comp.xwins.get_mut(&win) else {
        return;
    };
    let flag = match kind {
        XState::Fullscreen => &mut xw.fullscreen,
        XState::Maximized => &mut xw.maximized,
    };
    if on {
        if *flag {
            return; // already in that state
        }
        *flag = true;
        if xw.saved_geo.is_none() {
            xw.saved_geo = Some((xw.x, xw.y, xw.w, xw.h));
        }
        xw.x = rect.0;
        xw.y = rect.1;
        xw.w = rect.2;
        xw.h = rect.3;
    } else {
        *flag = false;
        // Restore only when no forcing state remains — a window
        // that is fullscreen+maximized drops to maximized, not
        // back to its floating spot.
        if !xw.fullscreen && !xw.maximized {
            if let Some((x, y, w, h)) = xw.saved_geo.take() {
                xw.x = x;
                xw.y = y;
                xw.w = w;
                xw.h = h;
            }
        }
    }
    let (x, y, w, h) = (xw.x, xw.y, xw.w, xw.h);
    let (fullscreen, maximized) = (xw.fullscreen, xw.maximized);
    // Mirror onto the wl_surface position — the scene renders
    // surfaces, not X windows.
    if let Some(sd) = surf.and_then(|s| s.data::<SurfaceData>()) {
        *sd.pos.lock().unwrap() = (x, y);
    }
    comp.xreqs.push(XReq::Configure {
        win,
        x,
        y,
        w: Some(w),
        h: Some(h),
    });
    comp.xreqs.push(XReq::SetState {
        win,
        fullscreen,
        maximized,
        minimized: surf
            .and_then(|s| s.data::<SurfaceData>())
            .map(|sd| sd.minimized.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false),
    });
    comp.damage_everything();
    info!(win, on, "xwm: window state applied");
}
