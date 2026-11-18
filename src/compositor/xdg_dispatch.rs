// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! xdg-shell request dispatch — the wire-level handlers for
//! xdg_wm_base, xdg_positioner, xdg_surface, xdg_toplevel and
//! xdg_popup. State and geometry helpers live in `xdg.rs`; this
//! file only translates protocol requests into them.

use tracing::info;
use wayland_protocols::xdg::shell::server::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::xdg::{resolve, Positioner};
use super::{CompState, Popup, SurfaceData};

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                // The owning wl_surface rides as resource data so
                // the xdg path can reach SurfaceData.
                init.init::<xdg_surface::XdgSurface, wl_surface::WlSurface>(id, surface);
            }
            xdg_wm_base::Request::CreatePositioner { id } => {
                init.init::<xdg_positioner::XdgPositioner, Positioner>(id, Positioner::default());
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, Positioner> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_positioner::XdgPositioner,
        request: xdg_positioner::Request,
        p: &Positioner,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_positioner::Request::SetSize { width, height } => {
                *p.size.lock().unwrap() = (width, height);
            }
            xdg_positioner::Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                *p.anchor_rect.lock().unwrap() = (x, y, width, height);
            }
            xdg_positioner::Request::SetAnchor { anchor } => {
                if let Ok(a) = anchor.into_result() {
                    *p.anchor.lock().unwrap() = a;
                }
            }
            xdg_positioner::Request::SetGravity { gravity } => {
                if let Ok(g) = gravity.into_result() {
                    *p.gravity.lock().unwrap() = g;
                }
            }
            xdg_positioner::Request::SetOffset { x, y } => {
                *p.offset.lock().unwrap() = (x, y);
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        xs: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                info!("client requested xdg toplevel");
                // The owning wl_surface rides as toplevel object
                // data — resolving it through `order` would drop
                // every request sent before the first commit (the
                // surface only joins order at map): set_title,
                // set_app_id and fullscreen all die there.
                let tl =
                    init.init::<xdg_toplevel::XdgToplevel, wl_surface::WlSurface>(id, surf.clone());
                if let Some(sd) = surf.data::<SurfaceData>() {
                    *sd.xdg_tl.lock().unwrap() = Some(tl.clone());
                    *sd.xdg_surf.lock().unwrap() = Some(xs.clone());
                    // SSD default — `decoration = server` frames
                    // every toplevel until it negotiates client-
                    // side; auto/client leave the flag off until
                    // the decoration object asks for it.
                    if state.cfg.deco == crate::config::DecoPref::Server {
                        sd.ssd.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                tl.configure(0, 0, Vec::new());
                xs.configure(state.next_serial_pub());
                info!("xdg toplevel configured");
            }
            xdg_surface::Request::GetPopup {
                id,
                parent,
                positioner,
            } => {
                // The popup object must ALWAYS be initialized —
                // leaving it without object data is a hard server
                // panic on next use. parent=None is legal: layer
                // clients create the popup here, then hand it to
                // zwlr_layer_surface.get_popup which swaps in the
                // real parent before the first commit.
                let pop = init.init::<xdg_popup::XdgPopup, ()>(id, ());
                // The xdg_surface's resource data IS the wl_surface
                // (stored at get_xdg_surface) — popup positions are
                // parent-relative in surface coordinates. The own
                // surface is the placeholder parent until a layer
                // surface claims the popup.
                let parent_surf = parent
                    .and_then(|xs| xs.data::<wl_surface::WlSurface>().cloned())
                    .unwrap_or_else(|| surf.clone());
                let pos_data = positioner.data::<Positioner>();
                let parent_size = parent_surf
                    .data::<SurfaceData>()
                    .and_then(|sd| {
                        sd.shown_buf
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|b| b.data::<super::BufInfo>().map(|bi| (bi.w, bi.h)))
                    })
                    .unwrap_or((0, 0));
                let (rel, size) = pos_data
                    .map(|p| resolve(p, parent_size))
                    .unwrap_or(((0, 0), (0, 0)));
                info!(?rel, ?size, "xdg popup created");
                // Popups don't join the stacking order — they track
                // their parent and draw above everything.
                state.popups.push(Popup {
                    surf: surf.clone(),
                    parent: parent_surf,
                    obj: pop.clone(),
                    rel,
                    size,
                });
                // popup_done is dismissal — sent ONLY on
                // outside-click in ptr_button. Firing it here
                // would tell the toolkit to destroy the menu
                // the moment it is created.
                pop.configure(rel.0, rel.1, size.0, size.1);
                xs.configure(state.next_serial_pub());
            }
            xdg_surface::Request::AckConfigure { serial } => {
                info!(serial, "client acked configure");
            }
            xdg_surface::Request::SetWindowGeometry {
                x,
                y,
                width,
                height,
            } => {
                info!(x, y, width, height, "client set window geometry");
                if let Some(sd) = surf.data::<SurfaceData>() {
                    *sd.win_geom.lock().unwrap() = (x, y, width, height);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _tl: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        owner: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let owner = owner.clone();
        let Some(sd) = owner.data::<SurfaceData>() else {
            return;
        };
        match request {
            xdg_toplevel::Request::Move { serial, .. } => {
                // Interactive move grab — pointer delta from the
                // press point repositions the window until release.
                // A serial matching a touch down means the finger
                // pressed a CSD titlebar: position and release
                // come from the touch slot, not the pointer.
                let touch_serial = state.touch_serials.contains_key(&serial);
                let touch_slot = state
                    .touch_serials
                    .get(&serial)
                    .copied()
                    .filter(|s| state.touch_pts.contains_key(s));
                // A touch serial whose finger already lifted means
                // the press ended before the request arrived —
                // nothing to drag, so no grab arms at all.
                if touch_serial && touch_slot.is_none() {
                    return;
                }
                let (cx, cy) = touch_slot
                    .and_then(|s| state.touch_pts.get(&s))
                    .map(|p| (p.gx, p.gy))
                    .unwrap_or(state.ptr_pos);
                let (px, py) = *sd.pos.lock().unwrap();
                state.grab = Some(super::Grab {
                    surf: owner.clone(),
                    kind: super::GrabKind::Move {
                        dx: cx - px as f64,
                        dy: cy - py as f64,
                    },
                    touch: touch_slot,
                });
                info!("move grab armed");
            }
            xdg_toplevel::Request::Resize { serial, edges, .. } => {
                let touch_serial = state.touch_serials.contains_key(&serial);
                let touch_slot = state
                    .touch_serials
                    .get(&serial)
                    .copied()
                    .filter(|s| state.touch_pts.contains_key(s));
                if touch_serial && touch_slot.is_none() {
                    return;
                }
                let (cx, cy) = touch_slot
                    .and_then(|s| state.touch_pts.get(&s))
                    .map(|p| (p.gx, p.gy))
                    .unwrap_or(state.ptr_pos);
                let (px, py) = *sd.pos.lock().unwrap();
                // The resize rect is the WINDOW GEOMETRY, not the
                // buffer — shadow margins belong to the client and
                // must not enter the configure math (the spec
                // resizes in logical/geometry space). Unset geometry
                // falls back to the effective surface size.
                let (gx, gy, gw, gh) = *sd.win_geom.lock().unwrap();
                let (bw, bh) = if gw > 0 && gh > 0 {
                    (gw, gh)
                } else {
                    sd.shown_buf
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|b| b.data::<super::BufInfo>())
                        .map(|bi| (bi.w, bi.h))
                        .unwrap_or((0, 0))
                };
                let _ = (gx, gy);
                state.grab = Some(super::Grab {
                    surf: owner.clone(),
                    kind: super::GrabKind::Resize {
                        edges: edges
                            .into_result()
                            .unwrap_or(xdg_toplevel::ResizeEdge::None),
                        start: (px, py, bw, bh),
                        anchor: (cx, cy),
                    },
                    touch: touch_slot,
                });
                info!("resize grab armed");
            }
            xdg_toplevel::Request::SetMaximized => {
                state.set_maximized_state(sd, true);
                state.ft_changed(&owner);
                info!("toplevel maximized");
            }
            xdg_toplevel::Request::UnsetMaximized => {
                state.set_maximized_state(sd, false);
                state.ft_changed(&owner);
            }
            xdg_toplevel::Request::SetFullscreen { .. } => {
                state.set_fullscreen_state(sd, true);
                state.ft_changed(&owner);
                info!("toplevel fullscreen");
            }
            xdg_toplevel::Request::UnsetFullscreen => {
                state.set_fullscreen_state(sd, false);
                state.ft_changed(&owner);
            }
            // Iconify — same flag the foreign-toplevel path
            // drives; render, hit-test and focus cycling all
            // already honor it. If the window held the keyboard
            // focus it is handed to the next candidate.
            xdg_toplevel::Request::SetMinimized => {
                sd.minimized
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if state
                    .kbd_focus
                    .as_ref()
                    .is_some_and(|f| f.id() == owner.id())
                {
                    state.cycle_focus();
                }
                state.ft_changed(&owner);
            }
            xdg_toplevel::Request::SetParent { parent } => {
                // Same-client transient-for — save dialogs
                // stack above their window. Cross-client
                // parenting goes through xdg_foreign instead.
                // The toplevel's user data IS its owner surface
                // (set at get_toplevel) — one data() hop.
                let par = parent
                    .as_ref()
                    .and_then(|t| t.data::<wl_surface::WlSurface>())
                    .cloned();
                state.set_transient(&owner, par.as_ref());
            }
            xdg_toplevel::Request::SetTitle { title } => {
                *sd.title.lock().unwrap() = title;
                state.ft_changed(&owner);
            }
            xdg_toplevel::Request::SetAppId { app_id } => {
                *sd.app_id.lock().unwrap() = app_id;
                state.ft_changed(&owner);
            }
            // The client's own resize bounds — stored for the
            // interactive grab, which clamps its computed rect
            // to them — the bounds are the client's own.
            xdg_toplevel::Request::SetMinSize { width, height } => {
                *sd.min_size.lock().unwrap() = (width, height);
            }
            xdg_toplevel::Request::SetMaxSize { width, height } => {
                *sd.max_size.lock().unwrap() = (width, height);
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        pop: &xdg_popup::XdgPopup,
        request: xdg_popup::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_popup::Request::Grab { .. } => {
                // Popup grab — keep the popup focused while open;
                // dismissal happens on outside click in ptr_button.
            }
            xdg_popup::Request::Destroy => {
                state.popups.retain(|p| p.obj.id() != pop.id());
                state.damage_everything();
            }
            _ => {}
        }
    }
}

// wl_seat dispatch lives in seat.rs — popups only carry it as
// a request argument in grab.
