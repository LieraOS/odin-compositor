// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Window management — stacking-order operations, focus cycling
//! and the per-output tiling policy.
//!
//! Tiling contract: a window belongs to the output containing its
//! position and tiles strictly inside that output's rect — tiled
//! windows never cross monitor boundaries (floating ones may).

use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::protocol::wl_surface;
use wayland_server::Resource;

use super::{CompState, SurfaceData};

impl CompState {
    /// Alt-Tab — rotate the bottom normal surface to the top and
    /// hand it the keyboard focus. Cursor-role surfaces never take
    /// part: they are not windows.
    pub fn cycle_focus(&mut self) {
        self.prune_dead();
        // A window participates only while visible — cycling to a
        // minimized one would focus a surface that isn't drawn.
        // Snapshot the visible workspace ids once — the closure
        // below can't borrow self while order mutates.
        let visible_ws: Vec<u8> = self
            .layout
            .outputs
            .values()
            .filter(|o| o.enabled)
            .map(|o| o.ws)
            .collect();
        let focusable = |s: &wl_surface::WlSurface| {
            s.data::<SurfaceData>()
                .map(|sd| {
                    !sd.is_cursor.load(std::sync::atomic::Ordering::SeqCst)
                        && !sd.minimized.load(std::sync::atomic::Ordering::SeqCst)
                        && visible_ws.contains(&*sd.ws.lock().unwrap())
                })
                .unwrap_or(false)
        };
        let pos = self.order.iter().position(&focusable);
        if let Some(idx) = pos {
            let s = self.order.remove(idx);
            self.order.push(s);
            self.restack_transients();
        }
        let top = self.order.iter().rev().find(|s| focusable(s)).cloned();
        self.set_kbd_focus(top);
        self.damage_everything();
    }

    /// Live floating↔tiling flip for the output under the
    /// pointer — every monitor owns its policy independently
    /// ("this screen tiles"), never a global mode. apply_layout
    /// then tiles the flipped-to-tiling output and restores
    /// floating geometry on a flipped-to-floating one.
    pub fn toggle_layout(&mut self) {
        let (px, py) = (self.ptr_pos.0 as i32, self.ptr_pos.1 as i32);
        let Some(name) = self
            .layout
            .outputs
            .values()
            .find(|o| {
                o.enabled
                    && px >= o.x
                    && px < o.x + o.mode.0 as i32
                    && py >= o.y
                    && py < o.y + o.mode.1 as i32
            })
            .or_else(|| {
                self.layout
                    .outputs
                    .values()
                    .find(|o| o.primary && o.enabled)
            })
            .map(|o| o.name.clone())
        else {
            return;
        };
        let Some(out) = self.layout.outputs.get_mut(&name) else {
            return;
        };
        out.policy = out.policy.toggle();
        info!(output = %name, policy = ?out.policy, "layout policy switched");
        self.apply_layout();
        self.flush_clients();
    }

    /// Re-apply user settings to the live layout — called at
    /// startup and on every config reload (bindings.conf changed
    /// under us). Primary flips only the flag: positions stay
    /// where output-management or auto-layout put them — a flag
    /// change must never clobber a user-arranged monitor setup.
    /// Policies: explicit `policy.<name>` wins, else the
    /// `policy =` all-monitors default; outputs the config says
    /// nothing about keep their runtime policy.
    pub fn apply_cfg(&mut self) {
        if let Some(p) = self.cfg.primary.clone() {
            if self.layout.outputs.contains_key(&p) {
                for o in self.layout.outputs.values_mut() {
                    o.primary = o.name == p;
                }
            }
        }
        let def = self.cfg.default_policy;
        if let Some(pol) = def {
            // The configured default also becomes the layout's
            // baseline — outputs added later inherit it even if
            // they have no explicit config entry.
            self.layout.policy = pol;
        }
        for (name, o) in self.layout.outputs.iter_mut() {
            if let Some(pol) = self.cfg.out_policy.get(name).copied().or(def) {
                o.policy = pol;
            }
        }
    }

    /// The window runs fullscreen — xdg state on native clients,
    /// _NET_WM_STATE_FULLSCREEN mirrored for X windows. Games and
    /// players stay out of the tiling grid by contract.
    pub(crate) fn is_fullscreen(&self, surf: &wl_surface::WlSurface) -> bool {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return false;
        };
        if sd
            .tl_states
            .lock()
            .unwrap()
            .contains(&xdg_toplevel::State::Fullscreen)
        {
            return true;
        }
        if let Some(wid) = *sd.xwin.lock().unwrap() {
            return self.xwins.get(&wid).map(|x| x.fullscreen).unwrap_or(false);
        }
        false
    }

    /// Hand a window back to the floating world: clear its tile
    /// flag and restore either the saved geometry (policy switch
    /// — the window returns to where the user last floated it)
    /// or just the natural size (drag-out — it stays where the
    /// user dropped it).
    fn float_restore(&mut self, surf: &wl_surface::WlSurface, restore_pos: bool) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        sd.tiled.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(wid) = *sd.xwin.lock().unwrap() {
            if let Some(xw) = self.xwins.get_mut(&wid) {
                if let Some((x, y, w, h)) = xw.saved_geo.take() {
                    if restore_pos {
                        xw.x = x;
                        xw.y = y;
                    }
                    // The natural size always comes back — the
                    // user expects their window's own dimensions
                    // again once it leaves the grid.
                    xw.w = w;
                    xw.h = h;
                    *sd.pos.lock().unwrap() = (xw.x, xw.y);
                    self.xreqs.push(crate::xwm::XReq::Configure {
                        win: wid,
                        x: xw.x,
                        y: xw.y,
                        w: Some(w),
                        h: Some(h),
                    });
                }
            }
            return;
        }
        if restore_pos {
            if let Some(p) = sd.float_pos.lock().unwrap().take() {
                *sd.pos.lock().unwrap() = p;
            }
        }
        // Explicit 0×0 = "client chooses" — reset the stored size
        // first so configure_tl doesn't keep the tile dimensions.
        *sd.cfg_size.lock().unwrap() = (0, 0);
        crate::compositor::xdg::configure_tl(self, sd, 0, 0);
    }

    /// A move grab just released — the drop output's policy takes
    /// over the window: landing on a Tiling monitor folds it into
    /// that grid (apply_layout assigns its slot), landing a tiled
    /// window on a Floating monitor frees it at the drop point.
    pub(crate) fn drop_policy(&mut self, surf: &wl_surface::WlSurface) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        let tiled = sd.tiled.load(std::sync::atomic::Ordering::SeqCst);
        let Some(out) = self.output_of(surf) else {
            return;
        };
        match out.policy {
            crate::wm::LayoutPolicy::Tiling => self.apply_layout(),
            crate::wm::LayoutPolicy::Floating if tiled => {
                self.float_restore(surf, false);
            }
            _ => {}
        }
    }

    /// Mapped xdg toplevels in stacking order — cursor and aux
    /// surfaces never carry a toplevel role object.
    /// Same as toplevels, but only windows on a workspace some
    /// output shows — tiling and focus cycling live here.
    fn ws_toplevels(&self) -> Vec<wl_surface::WlSurface> {
        self.toplevels()
            .into_iter()
            .filter(|s| self.ws_visible(s))
            .collect()
    }

    fn toplevels(&self) -> Vec<wl_surface::WlSurface> {
        self.order
            .iter()
            .filter(|s| {
                s.data::<SurfaceData>()
                    .map(|sd| {
                        CompState::has_window_role(sd)
                            && sd.mapped.load(std::sync::atomic::Ordering::SeqCst)
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// The output rect containing a window — membership decides
    /// by the window's current position; fresh windows (pos 0,0)
    /// and windows straddling boundaries go to whichever output
    /// holds their center, falling back to primary.
    pub(crate) fn output_of(&self, surf: &wl_surface::WlSurface) -> Option<crate::wm::Output> {
        let sd = surf.data::<SurfaceData>()?;
        let (px, py) = *sd.pos.lock().unwrap();
        let (bw, bh) = sd
            .shown_buf
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.data::<super::BufInfo>())
            .map(|bi| (bi.w, bi.h))
            .unwrap_or((0, 0));
        let cx = px + bw / 2;
        let cy = py + bh / 2;
        self.layout
            .outputs
            .values()
            .find(|o| {
                cx >= o.x && cx < o.x + o.mode.0 as i32 && cy >= o.y && cy < o.y + o.mode.1 as i32
            })
            .cloned()
            .or_else(|| {
                self.layout
                    .outputs
                    .values()
                    .find(|o| o.primary)
                    .or_else(|| self.layout.outputs.values().next())
                    .cloned()
            })
    }

    /// Re-arrange all toplevels under each output's own policy —
    /// a window obeys the policy of the monitor holding its
    /// center: Tiling outputs slot their members into the grid,
    /// Floating outputs hand previously-tiled members their
    /// saved geometry back. Fullscreen clients sit the grid out
    /// by contract (games, video players).
    pub fn apply_layout(&mut self) {
        self.damage_everything();
        // Only visible-workspace windows participate — hidden
        // ones hold their geometry until the workspace returns.
        let wins = self.ws_toplevels();
        // Group by the output containing each window's CENTER —
        // position decides membership, so a dragged window joins
        // the monitor it was dropped on.
        let mut groups: std::collections::BTreeMap<
            String,
            (crate::wm::LayoutPolicy, Vec<wl_surface::WlSurface>),
        > = Default::default();
        for w in &wins {
            if self.is_fullscreen(w) {
                continue;
            }
            if let Some(out) = self.output_of(w) {
                groups
                    .entry(out.name)
                    .or_insert_with(|| (out.policy, Vec::new()))
                    .1
                    .push(w.clone());
            }
        }
        for (name, (policy, members)) in groups {
            if !self.layout.outputs.contains_key(&name) {
                continue;
            }
            if policy == crate::wm::LayoutPolicy::Floating {
                // Floating output — freed windows get their saved
                // floating geometry back; already-floating ones
                // are left exactly where the user put them.
                for surf in &members {
                    let was_tiled = surf
                        .data::<SurfaceData>()
                        .map(|sd| sd.tiled.load(std::sync::atomic::Ordering::SeqCst))
                        .unwrap_or(false);
                    if was_tiled {
                        self.float_restore(surf, true);
                    }
                }
                continue;
            }
            // Layer-shell exclusive zones (panels) shrink THIS
            // output's usable rect — tiles must never slide under
            // a panel, and every window tiles inside its own
            // monitor, not the primary's.
            let rect = self.usable_rect_of(&name);
            let tiles = crate::wm::tiles(rect, members.len());
            for (surf, tile) in members.iter().zip(tiles.iter()) {
                let Some(sd) = surf.data::<SurfaceData>() else {
                    continue;
                };
                sd.tiled.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(wid) = *sd.xwin.lock().unwrap() {
                    // X window — no xdg configure; save the pre-tile
                    // geometry once, move the X window itself and
                    // point the surface at the tile.
                    if let Some(xw) = self.xwins.get_mut(&wid) {
                        if xw.saved_geo.is_none() {
                            xw.saved_geo = Some((xw.x, xw.y, xw.w, xw.h));
                        }
                        xw.x = tile.x;
                        xw.y = tile.y;
                        xw.w = tile.w;
                        xw.h = tile.h;
                    }
                    *sd.pos.lock().unwrap() = (tile.x, tile.y);
                    self.xreqs.push(crate::xwm::XReq::Configure {
                        win: wid,
                        x: tile.x,
                        y: tile.y,
                        w: Some(tile.w),
                        h: Some(tile.h),
                    });
                } else {
                    {
                        let mut fp = sd.float_pos.lock().unwrap();
                        if fp.is_none() {
                            *fp = Some(*sd.pos.lock().unwrap());
                        }
                        // SSD frames take the tile's outer margin
                        // — the tile rect covers decoration AND
                        // content, so pos/configure shrink by
                        // titlebar + borders on every side.
                        let ssd = self.ssd_on(sd);
                        let (bx, by) =
                            (super::ssd::BORDER, super::ssd::TITLE_H + super::ssd::BORDER);
                        *sd.pos.lock().unwrap() = if ssd {
                            (tile.x + bx, tile.y + by)
                        } else {
                            (tile.x, tile.y)
                        };
                        let (cw, ch) = if ssd {
                            (
                                (tile.w as i32 - super::ssd::BORDER * 2).max(64),
                                (tile.h as i32 - super::ssd::TITLE_H - super::ssd::BORDER * 2)
                                    .max(64),
                            )
                        } else {
                            (tile.w as i32, tile.h as i32)
                        };
                        crate::compositor::xdg::configure_tl(self, sd, cw, ch);
                    }
                }
                info!(
                    output = %name,
                    x = tile.x,
                    y = tile.y,
                    w = tile.w,
                    h = tile.h,
                    "retile configure"
                );
            }
        }
        self.flush_clients();
    }
}
