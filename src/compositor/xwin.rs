// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X window bookkeeping — the compositor-side mirror of one
//! managed X11 window: geometry, surface pairing, EWMH state.

/// One X window's compositor-side state — geometry in global
/// space plus the wl_surface pairing once WL_SURFACE_ID lands.
#[derive(Debug, Clone)]
pub struct Xwin {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// wl_surface protocol id inside Xwayland's client namespace.
    pub surface_id: Option<u32>,
    /// True once the wl_surface with that id actually showed up
    /// and got its xwin backlink — pending until then.
    pub associated: bool,
    pub mapped: bool,
    /// _NET_WM_STATE flags we applied — mirrored into the window's
    /// property so X clients can read their real state.
    pub fullscreen: bool,
    pub maximized: bool,
    /// Geometry before a compositor-forced resize (fullscreen,
    /// maximize, tile assign) — restored when the forcing state
    /// ends; the X client resizes itself on our configure.
    pub saved_geo: Option<(i32, i32, u32, u32)>,
    /// Override-redirect (menus/tooltips) — bypassed the redirect.
    /// Parsed at map time; consumed when X11 unmanaged windows
    /// get their floating/no-focus treatment (XWM follow-up).
    #[allow(dead_code)]
    pub override_redirect: bool,
}
