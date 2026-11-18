// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X11 ClientMessage dispatch — WL_SURFACE_ID association,
//! EWMH activation/state requests and ICCCM minimize.

use tracing::info;
use wayland_server::Resource;
use x11rb::protocol::xproto::ClientMessageEvent;

use super::assoc::{associate, surface_of};
use super::Xwm;
use crate::compositor::{CompState, Grab, GrabKind, SurfaceData};

/// One ClientMessage from an X client — association, activation
/// and window-state requests all travel this channel.
pub fn handle(xwm: &mut Xwm, comp: &mut CompState, e: ClientMessageEvent) {
    if e.type_ == xwm.atoms.wl_surface_id {
        // The surface id travels as lo/hi pair in
        // data32[0]/[1] — [0] holds the protocol id
        // inside Xwayland's own client namespace.
        let d = e.data.as_data32();
        let sid = d[0] as u64 | ((d[1] as u64) << 32);
        associate(xwm, comp, e.window, sid as u32);
    } else if e.type_ == xwm.atoms.net_active_window {
        // EWMH activation request — the X client asks
        // to be focused; e.window is the target.
        let sid = comp.xwins.get(&e.window).and_then(|x| x.surface_id);
        if let Some(surf) = surface_of(comp, sid) {
            comp.raise(&surf);
            comp.set_kbd_focus(Some(surf));
            info!(win = e.window, "xwm: activate request honored");
        }
    } else if e.type_ == xwm.atoms.net_moveresize {
        // EWMH client-initiated move/resize — the client owns its
        // titlebar visuals, received the press, and asks the WM
        // to drive the drag (Steam's titlebar does exactly this).
        // data32 = [x_root, y_root, direction, button, source].
        let d = e.data.as_data32();
        let (rx, ry, dir) = (d[0] as i32, d[1] as i32, d[2]);
        let xw = comp.xwins.get(&e.window);
        // Override-redirect windows are not WM-managed — a stray
        // moveresize from one must not arm a grab.
        if xw.map(|x| !x.override_redirect).unwrap_or(false) {
            if let Some(surf) = surface_of(comp, xw.and_then(|x| x.surface_id)) {
                if let Some(sd) = surf.data::<SurfaceData>() {
                    let (px, py) = *sd.pos.lock().unwrap();
                    let kind = match dir {
                        // _NET_WM_MOVERESIZE_MOVE — the press
                        // offset inside the window keeps the drag
                        // anchored where the user grabbed it.
                        8 => Some(GrabKind::Move {
                            dx: rx as f64 - px as f64,
                            dy: ry as f64 - py as f64,
                        }),
                        // Directions 0–7 are resize edges in EWMH
                        // order (top-left … left), matching
                        // xdg_toplevel::ResizeEdge semantics.
                        0..=7 => {
                            use wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as E;
                            let edges = match dir {
                                0 => E::TopLeft,
                                1 => E::Top,
                                2 => E::TopRight,
                                3 => E::Right,
                                4 => E::BottomRight,
                                5 => E::Bottom,
                                6 => E::BottomLeft,
                                _ => E::Left,
                            };
                            let (bw, bh) = sd
                                .shown_buf
                                .lock()
                                .unwrap()
                                .as_ref()
                                .and_then(|b| b.data::<crate::compositor::BufInfo>())
                                .map(|bi| (bi.w, bi.h))
                                .unwrap_or((0, 0));
                            Some(GrabKind::Resize {
                                edges,
                                start: (px, py, bw, bh),
                                anchor: (rx as f64, ry as f64),
                            })
                        }
                        // 9/10 keyboard-driven, 11 cancel — ignored
                        // like every other compositor does.
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        comp.raise(&surf);
                        comp.grab = Some(Grab {
                            surf,
                            kind,
                            touch: None,
                        });
                        info!(win = e.window, dir, "xwm: moveresize grab armed");
                    }
                }
            }
        }
    } else if e.type_ == xwm.atoms.wm_change_state {
        // ICCCM minimize: data32[0]==3 is IconicState —
        // our minimized flag hides it like a wl window.
        let d = e.data.as_data32();
        if d[0] == 3 {
            let sid = comp.xwins.get(&e.window).and_then(|x| x.surface_id);
            if let Some(surf) = surface_of(comp, sid) {
                if let Some(sd) = surf.data::<SurfaceData>() {
                    sd.minimized
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            comp.damage_everything();
            comp.apply_layout();
            info!(win = e.window, "xwm: window minimized");
        }
    } else if e.type_ == xwm.atoms.net_wm_state {
        // EWMH state request: data32 = action, atom1,
        // atom2 — 0=remove 1=add 2=toggle.
        let d = e.data.as_data32();
        let [action, a1, a2, _, _] = d;
        let surf = surface_of(comp, comp.xwins.get(&e.window).and_then(|x| x.surface_id));
        for atom in [a1, a2] {
            let kind = if atom == xwm.atoms.net_wm_state_fullscreen {
                Some(super::state::XState::Fullscreen)
            } else if atom == xwm.atoms.net_wm_state_max_vert
                || atom == xwm.atoms.net_wm_state_max_horz
            {
                Some(super::state::XState::Maximized)
            } else {
                None
            };
            let Some(kind) = kind else { continue };
            let cur = comp
                .xwins
                .get(&e.window)
                .map(|x| match kind {
                    super::state::XState::Fullscreen => x.fullscreen,
                    super::state::XState::Maximized => x.maximized,
                })
                .unwrap_or(false);
            let on = match action {
                1 => true,
                2 => !cur,
                _ => false,
            };
            super::state::set_xwin_state(comp, e.window, kind, on, surf.as_ref());
        }
    }
}
