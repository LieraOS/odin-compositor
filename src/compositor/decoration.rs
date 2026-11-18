// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Decoration negotiation — `zxdg_decoration_manager_v1` plus
//! the KDE `server_decoration` twin GTK/Qt clients still bind.
//!
//! Odin renders no server-side frame, so the honest answer to
//! every negotiation is `client_side`/`Client` — a client that
//! asks for SSD still gets a definitive configure instead of
//! silence (protocol: the server picks, and undecided servers
//! break Qt's frame heuristics).

use wayland_protocols::xdg::decoration::zv1::server::{
    zxdg_decoration_manager_v1 as dm, zxdg_toplevel_decoration_v1 as td,
};
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_protocols_misc::server_decoration::server::{
    org_kde_kwin_server_decoration as kde_dec, org_kde_kwin_server_decoration_manager as kde_mgr,
    org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::xdg::decoration::zv1::server::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
pub use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1;
pub use wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager as KdeDecoManager;

/// Request data on every xdg decoration object — the toplevel
/// it decorates, so `already_constructed` can be enforced.
pub struct DecoData {
    pub toplevel: xdg_toplevel::XdgToplevel,
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        mgr: &ZxdgDecorationManagerV1,
        request: dm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let dm::Request::GetToplevelDecoration { id, toplevel } = request {
            {
                // The new object must exist even on error paths —
                // an uninit'd new-id panics the wayland backend and
                // takes the whole compositor down with it.
                let deco =
                    init.init::<ZxdgToplevelDecorationV1, DecoData>(id, DecoData { toplevel });
                if !deco.data::<DecoData>().unwrap().toplevel.is_alive() {
                    mgr.post_error(
                        td::Error::Orphaned as u32,
                        "toplevel destroyed before decoration request",
                    );
                    return;
                }
                // One decoration object per toplevel — a second
                // create is a protocol error, not a second copy.
                let tl_id = deco.data::<DecoData>().unwrap().toplevel.id();
                if state.deco_objs.iter().any(|o| {
                    o.is_alive()
                        && o.data::<DecoData>()
                            .is_some_and(|d| d.toplevel.id() == tl_id)
                }) {
                    mgr.post_error(
                        td::Error::AlreadyConstructed as u32,
                        "toplevel already has a decoration object",
                    );
                    return;
                }
                // Negotiation answer up front — clients block on
                // this before deciding CSD vs none. `auto` lets
                // the compositor pick: SSD is ours to draw, so
                // undecided clients get a frame by default.
                let mode = match state.cfg.deco {
                    crate::config::DecoPref::Client => td::Mode::ClientSide,
                    _ => td::Mode::ServerSide,
                };
                state.set_ssd(
                    &deco.data::<DecoData>().unwrap().toplevel,
                    mode == td::Mode::ServerSide,
                );
                deco.configure(mode);
                state.deco_objs.push(deco);
            }
        }
    }
}

impl CompState {
    /// Record a decoration decision on the surface — the SSD
    /// flag drives frame drawing, hit-testing and the tiling
    /// margin math. A mode flip repaints the whole decorated
    /// extent so the new frame appears (or disappears) at once.
    fn set_ssd(&mut self, toplevel: &xdg_toplevel::XdgToplevel, ssd: bool) {
        // The xdg_toplevel's user data IS the wl_surface —
        // stored at `get_toplevel`, so pre-map requests work.
        let Some(surf) = toplevel.data::<wayland_server::protocol::wl_surface::WlSurface>() else {
            return;
        };
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        if sd.ssd.swap(ssd, std::sync::atomic::Ordering::SeqCst) == ssd {
            return; // nothing changed — no repaint needed
        }
        self.damage_everything();
    }

    /// Shared negotiation: pick the mode to answer for a client
    /// request — `server` always frames, `client` never does,
    /// `auto` honors the request and defaults undecided to SSD.
    fn pick_mode(&self, requested: Option<td::Mode>) -> td::Mode {
        match self.cfg.deco {
            crate::config::DecoPref::Server => td::Mode::ServerSide,
            crate::config::DecoPref::Client => td::Mode::ClientSide,
            crate::config::DecoPref::Auto => requested.unwrap_or(td::Mode::ServerSide),
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, DecoData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        deco: &ZxdgToplevelDecorationV1,
        request: td::Request,
        data: &DecoData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            td::Request::SetMode { mode } => {
                // The configured mode is the negotiation's
                // answer: `server` keeps our frame regardless,
                // `client` never draws one, `auto` honors the
                // request.
                let want = mode.into_result().ok();
                let answer = state.pick_mode(want);
                state.set_ssd(&data.toplevel, answer == td::Mode::ServerSide);
                deco.configure(answer);
            }
            td::Request::UnsetMode => {
                // Undecided client — the server picks, which is
                // SSD everywhere but `client` mode.
                let answer = state.pick_mode(None);
                state.set_ssd(&data.toplevel, answer == td::Mode::ServerSide);
                deco.configure(answer);
            }
            _ => {}
        }
    }
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &OrgKdeKwinServerDecorationManager,
        request: kde_mgr::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let kde_mgr::Request::Create { id, surface } = request {
            // The wl_surface rides as object data so later
            // RequestMode calls can flip its ssd flag.
            let deco = init.init::<
                wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
                wayland_server::protocol::wl_surface::WlSurface,
            >(id, surface);
            // KDE side: the mode event is the negotiation answer
            // — same policy as the xdg twin.
            let server = !matches!(state.cfg.deco, crate::config::DecoPref::Client);
            if let Some(sd) = deco
                .data::<wayland_server::protocol::wl_surface::WlSurface>()
                .and_then(|s| s.data::<SurfaceData>())
            {
                sd.ssd.store(server, std::sync::atomic::Ordering::SeqCst);
            }
            deco.mode(if server {
                kde_dec::Mode::Server
            } else {
                kde_dec::Mode::Client
            });
        }
    }
}

impl Dispatch<
    wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
    wayland_server::protocol::wl_surface::WlSurface,
> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        deco: &wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        request: kde_dec::Request,
        surf: &wayland_server::protocol::wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // release is the destructor; request_mode answers under
        // the same policy as the xdg negotiation.
        if let kde_dec::Request::RequestMode { mode } = request {
            let want_server = mode
                .into_result()
                .is_ok_and(|m| matches!(m, kde_dec::Mode::Server));
            let server = match state.cfg.deco {
                crate::config::DecoPref::Server => true,
                crate::config::DecoPref::Client => false,
                crate::config::DecoPref::Auto => want_server,
            };
            if let Some(sd) = surf.data::<SurfaceData>() {
                if sd.ssd.swap(server, std::sync::atomic::Ordering::SeqCst) != server {
                    state.damage_everything();
                }
            }
            deco.mode(if server {
                kde_dec::Mode::Server
            } else {
                kde_dec::Mode::Client
            });
        }
    }
}
