// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! xdg-shell — toplevel windows, popups and the positioner that
//! places them relative to a parent.
//!
//! Toplevels carry state (activated/maximized/fullscreen) driven
//! by focus + client requests. Popups attach to a parent surface
//! at a positioner-computed offset; they live in `state.popups`,
//! draw above all toplevels and dismiss on outside click.

use wayland_protocols::xdg::shell::server::{xdg_positioner, xdg_toplevel};
use wayland_server::protocol::wl_surface;
use wayland_server::Resource;

use super::{CompState, SurfaceData};

/// Positioner state — the client fills this before get_popup;
/// at popup creation we resolve it to a parent-relative offset.
/// Anchor/Gravity have no Default impl upstream, hence manual.
pub(crate) struct Positioner {
    pub size: std::sync::Mutex<(i32, i32)>,
    pub anchor_rect: std::sync::Mutex<(i32, i32, i32, i32)>,
    pub anchor: std::sync::Mutex<xdg_positioner::Anchor>,
    pub gravity: std::sync::Mutex<xdg_positioner::Gravity>,
    pub offset: std::sync::Mutex<(i32, i32)>,
}

impl Default for Positioner {
    fn default() -> Self {
        Self {
            size: Default::default(),
            anchor_rect: Default::default(),
            anchor: std::sync::Mutex::new(xdg_positioner::Anchor::None),
            gravity: std::sync::Mutex::new(xdg_positioner::Gravity::None),
            offset: Default::default(),
        }
    }
}

/// Resolve a positioner to a popup offset relative to the parent's
/// surface-local origin + the popup's size.
///
/// Anchor semantics (xdg-shell): the anchor edge/point of the
/// anchor_rect is where the popup's gravity-opposite edge lands.
/// We implement the common cases (None, corner + edge anchors,
/// flipped gravity) — the same subset every toolbox menu uses.
pub(crate) fn resolve(p: &Positioner, parent_size: (i32, i32)) -> ((i32, i32), (i32, i32)) {
    let (w, h) = *p.size.lock().unwrap();
    let (rx, ry, rw, rh) = *p.anchor_rect.lock().unwrap();
    // Default anchor rect: the whole parent surface.
    let (rx, ry, rw, rh) = if rw == 0 && rh == 0 {
        (0, 0, parent_size.0, parent_size.1)
    } else {
        (rx, ry, rw, rh)
    };
    let anchor = *p.anchor.lock().unwrap();
    use xdg_positioner::Anchor as A;
    // Point on the anchor rect the popup attaches to.
    let (ax, ay) = match anchor {
        A::Top | A::TopLeft | A::TopRight => (anchor_x(anchor, rx, rw), ry),
        A::Bottom | A::BottomLeft | A::BottomRight => (anchor_x(anchor, rx, rw), ry + rh),
        A::Left | A::Right => (anchor_x(anchor, rx, rw), ry + rh / 2),
        _ => (rx + rw / 2, ry + rh / 2),
    };
    let (ox, oy) = *p.offset.lock().unwrap();
    let gravity = *p.gravity.lock().unwrap();
    use xdg_positioner::Gravity as G;
    // Which corner of the popup sits on the anchor point —
    // gravity pulls the popup away from it.
    let (px, py) = match gravity {
        G::Bottom | G::BottomLeft | G::BottomRight => (ax - gravity_dx(gravity, w), ay),
        G::Top | G::TopLeft | G::TopRight => (ax - gravity_dx(gravity, w), ay - h),
        G::Left | G::Right => (ax - gravity_dx(gravity, w), ay - h / 2),
        _ => (ax - w / 2, ay - h / 2),
    };
    ((px + ox, py + oy), (w, h))
}

fn anchor_x(a: xdg_positioner::Anchor, rx: i32, rw: i32) -> i32 {
    use xdg_positioner::Anchor as A;
    match a {
        A::TopLeft | A::BottomLeft | A::Left => rx,
        A::TopRight | A::BottomRight | A::Right => rx + rw,
        _ => rx + rw / 2,
    }
}

fn gravity_dx(g: xdg_positioner::Gravity, w: i32) -> i32 {
    use xdg_positioner::Gravity as G;
    match g {
        G::TopLeft | G::BottomLeft | G::Left => 0,
        G::TopRight | G::BottomRight | G::Right => w,
        _ => w / 2,
    }
}

/// Re-send configure to a toplevel with its state set — used for
/// activated-on-focus, maximize/fullscreen and interactive resize.
/// (0,0) means "keep the size we last configured" — tiling calls
/// this from state changes and must NOT un-pin the tile size.
impl CompState {
    /// Maximize/unmaximize — shared by the xdg request and the
    /// foreign-toplevel handle request (taskbars drive both).
    pub(crate) fn set_maximized_state(&mut self, sd: &SurfaceData, on: bool) {
        // X windows take the X-state path — no xdg configure
        // exists for them.
        if let Some(wid) = *sd.xwin.lock().unwrap() {
            let surf = self
                .order
                .iter()
                .find(|s| {
                    s.data::<SurfaceData>()
                        .is_some_and(|d| *d.xwin.lock().unwrap() == Some(wid))
                })
                .cloned();
            crate::xwm::state::set_xwin_state(
                self,
                wid,
                crate::xwm::state::XState::Maximized,
                on,
                surf.as_ref(),
            );
            return;
        }
        if on {
            {
                let mut st = sd.tl_states.lock().unwrap();
                if !st.contains(&xdg_toplevel::State::Maximized) {
                    st.push(xdg_toplevel::State::Maximized);
                }
            }
            // Save the pre-maximize spot BEFORE moving — unset
            // restores it so the window lands where it was.
            let cur = *sd.pos.lock().unwrap();
            *sd.float_pos.lock().unwrap() = Some(cur);
            let (rx, ry, rw, rh) = self.own_output_rect(sd);
            // SSD frames keep their margins when maximized —
            // the output rect covers decoration AND content.
            let (bx, by) = if self.ssd_on(sd) {
                (super::ssd::BORDER, super::ssd::TITLE_H + super::ssd::BORDER)
            } else {
                (0, 0)
            };
            *sd.pos.lock().unwrap() = (rx + bx, ry + by);
            configure_tl(
                self,
                sd,
                (rw as i32 - bx * 2).max(64),
                (rh as i32 - by - super::ssd::BORDER).max(64),
            );
        } else {
            sd.tl_states
                .lock()
                .unwrap()
                .retain(|s| *s != xdg_toplevel::State::Maximized);
            if let Some(p) = sd.float_pos.lock().unwrap().take() {
                *sd.pos.lock().unwrap() = p;
            }
            *sd.cfg_size.lock().unwrap() = (0, 0);
            configure_tl(self, sd, 0, 0);
        }
    }

    /// The output rect the window lives on — center-based like
    /// apply_layout's membership, falling back to primary for
    /// windows whose center sits off every screen.
    fn own_output_rect(&self, sd: &SurfaceData) -> (i32, i32, u32, u32) {
        let surf = self
            .order
            .iter()
            .find(|s| s.data::<SurfaceData>().is_some_and(|d| std::ptr::eq(d, sd)))
            .cloned();
        surf.as_ref()
            .and_then(|s| self.output_of(s))
            .map(|o| (o.x, o.y, o.mode.0, o.mode.1))
            .unwrap_or_else(|| self.layout.primary_rect())
    }

    /// Fullscreen/unfullscreen — same shared-request case as
    /// maximize.
    pub(crate) fn set_fullscreen_state(&mut self, sd: &SurfaceData, on: bool) {
        if let Some(wid) = *sd.xwin.lock().unwrap() {
            let surf = self
                .order
                .iter()
                .find(|s| {
                    s.data::<SurfaceData>()
                        .is_some_and(|d| *d.xwin.lock().unwrap() == Some(wid))
                })
                .cloned();
            crate::xwm::state::set_xwin_state(
                self,
                wid,
                crate::xwm::state::XState::Fullscreen,
                on,
                surf.as_ref(),
            );
            return;
        }
        if on {
            {
                let mut st = sd.tl_states.lock().unwrap();
                if !st.contains(&xdg_toplevel::State::Fullscreen) {
                    st.push(xdg_toplevel::State::Fullscreen);
                }
            }
            // Fullscreen belongs on the output the window lives
            // on — a game launched on the right monitor fills
            // THAT screen, never the primary's rect.
            let (rx, ry, rw, rh) = self.own_output_rect(sd);
            *sd.pos.lock().unwrap() = (rx, ry);
            configure_tl(self, sd, rw as i32, rh as i32);
        } else {
            sd.tl_states
                .lock()
                .unwrap()
                .retain(|s| *s != xdg_toplevel::State::Fullscreen);
            *sd.cfg_size.lock().unwrap() = (0, 0);
            configure_tl(self, sd, 0, 0);
        }
    }
}

pub fn configure_tl(state: &mut CompState, sd: &SurfaceData, w: i32, h: i32) {
    let serial = state.next_serial_pub();
    let (w, h) = if w == 0 && h == 0 {
        *sd.cfg_size.lock().unwrap()
    } else {
        *sd.cfg_size.lock().unwrap() = (w, h);
        (w, h)
    };
    let mut state_vals: Vec<u32> = sd
        .tl_states
        .lock()
        .unwrap()
        .iter()
        .map(|s| (*s).into())
        .collect();
    if let Some(tl) = sd.xdg_tl.lock().unwrap().as_ref() {
        let ver = tl.version();
        // v6 `suspended`: the surface isn't being repainted —
        // minimized, or parked on a workspace no enabled output
        // shows. Computed per configure, never stored: the same
        // rules render and hit-test already apply.
        if ver >= 6 {
            let visible = state
                .layout
                .outputs
                .values()
                .any(|o| o.enabled && o.ws == *sd.ws.lock().unwrap());
            if !visible || sd.minimized.load(std::sync::atomic::Ordering::SeqCst) {
                state_vals.push(xdg_toplevel::State::Suspended.into());
            }
        }
        // v5 `wm_capabilities`: exactly once before the first
        // xdg_surface.configure. Honest list — window_menu is
        // not implemented, so it is not advertised.
        if ver >= 5
            && !sd
                .wm_caps_sent
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            tl.wm_capabilities([2u32, 3, 4].iter().flat_map(|v| v.to_le_bytes()).collect());
        }
        // v4 `configure_bounds`: suggested geometry ceiling =
        // the usable rect (post-exclusive-zones) of the output
        // the window lives on.
        if ver >= 4 {
            let bounds = tl
                .data::<wl_surface::WlSurface>()
                .and_then(|surf| state.output_of(surf))
                .map(|out| state.usable_rect_of(&out.name))
                .map(|(_, _, bw, bh)| (bw as i32, bh as i32))
                .unwrap_or((0, 0));
            tl.configure_bounds(bounds.0, bounds.1);
        }
        tl.configure(
            w,
            h,
            state_vals.iter().flat_map(|v| v.to_le_bytes()).collect(),
        );
    }
    if let Some(xs) = sd.xdg_surf.lock().unwrap().as_ref() {
        xs.configure(serial);
    }
}
