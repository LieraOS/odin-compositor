// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Dead-object sweeping — Wayland resources go stale the
//! moment their client dies; every dispatch cycle calls
//! prune_dead to drop them before they poison routing, focus
//! or protocol state.

use super::CompState;
use wayland_server::Resource;

impl CompState {
    /// Drop every resource whose client is gone — surfaces,
    /// seats, selection sources, protocol objects. Taskbars are
    /// notified first or their handles linger forever. Any
    /// removal damages the scene: the vacated pixels would
    /// otherwise keep their last-presented content forever —
    /// ghost windows outliving their clients.
    pub(crate) fn prune_dead(&mut self) {
        // Dead toplevels leave the scene quietly — tell the
        // taskbars first or their handles linger forever.
        let dead: Vec<_> = self
            .order
            .iter()
            .filter(|s| !s.is_alive())
            .cloned()
            .collect();
        for s in &dead {
            self.ft_closed(s);
        }
        let mut removed = !dead.is_empty();
        macro_rules! sweep {
            ($list:expr, @$clear:tt) => {{
                if $list.as_ref().is_some_and(|r| !r.is_alive()) {
                    $list = None;
                    removed = true;
                }
            }};
            ($list:expr) => {{
                let n = $list.len();
                $list.retain(|r| r.is_alive());
                removed |= $list.len() != n;
            }};
            ($list:expr, $f:expr) => {{
                let n = $list.len();
                $list.retain($f);
                removed |= $list.len() != n;
            }};
        }
        self.order.retain(|s| s.is_alive());
        sweep!(self.popups, |p: &super::Popup| {
            p.surf.is_alive() && p.parent.is_alive()
        });
        sweep!(self.xpopups);
        sweep!(self.pointers);
        sweep!(self.keyboards);
        sweep!(self.data_devices);
        sweep!(self.ctrl_devices);
        sweep!(self.all_surfaces);
        sweep!(self.rel_ptrs);
        sweep!(self.layers, |l: &super::layer::Layer| {
            l.surf.is_alive() && l.obj.is_alive()
        });
        sweep!(self.outputs, |(o, _): &(
            wayland_server::protocol::wl_output::WlOutput,
            _,
        )| { o.is_alive() });
        // A constraint whose surface died must go too — a stale
        // lock would freeze the cursor forever.
        if let Some(c) = &self.constraint {
            if !c.surf.is_alive() {
                self.constraint = None;
                removed = true;
            }
        }
        sweep!(self.focus, @clear);
        sweep!(self.kbd_focus, @clear);
        sweep!(self.selection, @clear);
        // A ramp whose control object died (client quit without
        // destroy) drops — the output falls back to linear.
        {
            let n = self.gamma.len();
            self.gamma.retain(|_, (ctl, _)| ctl.is_alive());
            removed |= self.gamma.len() != n;
        }
        // An inhibitor whose client died releases the grab —
        // same as an explicit destroy request.
        sweep!(self.inhibitor, @clear);
        sweep!(self.primary, @clear);
        sweep!(self.prim_devices);
        sweep!(self.touch_objs);
        sweep!(self.text_inputs);
        sweep!(self.im, @clear);
        sweep!(self.im_popups, |p: &super::ImPopup| {
            p.surf.is_alive() && p.obj.is_alive()
        });
        // xdg-decoration negotiation objects die with their
        // clients; the negotiated mode itself lives on the
        // toplevel, not here.
        sweep!(self.deco_objs);
        // Tablet seats + per-seat device objects — a dead seat
        // object drops its whole view of tablets/tools/pads.
        sweep!(self.tablet_seats);
        for t in &mut self.tablets {
            t.objs.retain(|(s, _)| s.is_alive());
        }
        for t in &mut self.tools {
            t.objs.retain(|(s, _)| s.is_alive());
            // A tool whose focus surface died loses focus — next
            // proximity re-hits instead of stranding events.
            if t.focused
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| !s.is_alive())
            {
                *t.focused.lock().unwrap() = None;
            }
        }
        for p in &mut self.pads {
            p.objs.retain(|(s, _)| s.is_alive());
        }
        sweep!(self.swipe_gestures);
        sweep!(self.pinch_gestures);
        sweep!(self.hold_gestures);
        // Touch points grabbed on a dead surface free their slots
        // — otherwise a killed client pins the slot forever.
        let n = self.touch_pts.len();
        self.touch_prune();
        removed |= self.touch_pts.len() != n;
        // Exported handles + imported objects whose surface
        // died — importeds get `destroyed`, children untransient.
        let n = self.exported.len() + self.imported.len();
        self.foreign_prune();
        removed |= self.exported.len() + self.imported.len() != n;
        if removed {
            self.damage_everything();
        }
    }
}
