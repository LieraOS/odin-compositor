// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Window-management core — the seam where layout policy plugs in.
//!
//! Two contracts this module carries from day one:
//! 1. **Primary monitor**: the user picks it in the desktop's
//!    display settings; the choice is live (wl_output ordering,
//!    X11 RandR primary, panel placement all follow without
//!    restart).
//! 2. **Layout policy**: floating (default) and tiling are
//!    interchangeable policies over one element model — switching
//!    is a live Settings toggle, never a restart.

use std::collections::BTreeMap;
use tracing::info;

/// One video mode the connector advertises — the list clients
/// pick from through wl_output mode events and
/// zwlr_output_head_v1 mode objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSpec {
    pub w: u32,
    pub h: u32,
    /// Refresh in Hz (DRM vrefresh).
    pub hz: u32,
    /// The monitor's native/preferred mode flag.
    pub preferred: bool,
}

/// Monitor identity decoded from the connector's EDID block —
/// make is the 3-letter PNP manufacturer id, model the monitor
/// name string (or product-code fallback), serial the EDID
/// serial. Empty strings when the monitor reports none — never
/// invented values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edid {
    pub make: String,
    pub model: String,
    pub serial: String,
}

/// A display output in the compositor's global space.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Output {
    /// Kernel connector name — "DP-3", "HDMI-A-1".
    pub name: String,
    /// Position in global space (pixels).
    pub x: i32,
    pub y: i32,
    /// Current mode (w×h, Hz).
    pub mode: (u32, u32, u32),
    /// Every mode the connector advertised at scan time —
    /// output-management clients may only pick from this list.
    pub modes: Vec<ModeSpec>,
    /// Physical panel size in millimetres (EDID/connector
    /// reported) — wl_output.geometry and head.physical_size
    /// advertise exactly this.
    pub phys_mm: (u32, u32),
    /// Monitor identity — make/model/serial for clients that
    /// persist per-monitor profiles (kanshi, wdisplays).
    pub edid: Edid,
    /// Whether the user marked this output primary.
    pub primary: bool,
    /// Live on/off switch (output-management disable). Disabled
    /// outputs keep their record but claim no space and get no
    /// scene, no tiles and no pointer area.
    pub enabled: bool,
    /// The workspace this output currently shows — workspaces
    /// own outputs (i3 model); a window is visible when its ws
    /// equals the ws of an enabled output.
    pub ws: u8,
    /// The workspace this output showed before the current one —
    /// used when another output steals this workspace, so the
    /// losing output falls back instead of going blank.
    pub prev_ws: u8,
    /// Fractional output scale — 1.0 until output-management
    /// applies a configured value. Drives wp_fractional_scale's
    /// preferred_scale event (×120 fixed point).
    pub scale: f64,
    /// This output's own layout policy — the user's per-monitor
    /// choice: Floating (default) or Tiling. A window obeys the
    /// policy of the output its center sits on.
    pub policy: LayoutPolicy,
}

/// Output layout — owns positioning + the primary contract.
#[allow(dead_code)]
pub struct Layout {
    pub outputs: BTreeMap<String, Output>,
    /// Default policy handed to every new output — the actual
    /// per-monitor policy lives on `Output.policy` and changes
    /// there (binding, config `policy.<name>`).
    pub policy: LayoutPolicy,
}

/// Window arrangement policies — both live on the same element
/// model; Settings picks one and it applies immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPolicy {
    /// Classic overlapping windows (default).
    Floating,
    /// Tiling wm-style splits.
    Tiling,
}

impl LayoutPolicy {
    /// Flip between the two policies — the binding/Settings toggle.
    pub fn toggle(self) -> Self {
        match self {
            Self::Floating => Self::Tiling,
            Self::Tiling => Self::Floating,
        }
    }
}

/// One tile assignment in global space — origin + extent the
/// client is asked to take (xdg_toplevel.configure size).
#[derive(Debug, Clone, Copy)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Master-stack tiling: the first window owns the left half at
/// full height, every further window shares the right half as a
/// vertical stack — the classic dwm arrangement. `rect` is the
/// output's global-space rectangle (x, y, w, h).
pub fn tiles(rect: (i32, i32, u32, u32), count: usize) -> Vec<Tile> {
    let (rx, ry, rw, rh) = rect;
    match count {
        0 => Vec::new(),
        1 => vec![Tile {
            x: rx,
            y: ry,
            w: rw,
            h: rh,
        }],
        n => {
            let master_w = rw / 2;
            let stack_h = rh / (n as u32 - 1).max(1);
            let mut out = Vec::with_capacity(n);
            out.push(Tile {
                x: rx,
                y: ry,
                w: master_w,
                h: rh,
            });
            for i in 1..n {
                out.push(Tile {
                    x: rx + master_w as i32,
                    y: ry + ((i as u32 - 1) * stack_h) as i32,
                    w: rw - master_w,
                    h: if i == n - 1 {
                        rh - (i as u32 - 1) * stack_h
                    } else {
                        stack_h
                    },
                });
            }
            out
        }
    }
}

#[allow(dead_code)]
impl Layout {
    pub fn new() -> Self {
        Self {
            outputs: BTreeMap::new(),
            policy: LayoutPolicy::Floating,
        }
    }

    /// Insert an output and relayout: primary always at x=0,
    /// others arranged right of it in connector order. Settings
    /// positions replace this auto-layout once phase 3 lands.
    pub fn add(
        &mut self,
        name: String,
        mode: (u32, u32, u32),
        phys_mm: (u32, u32),
        edid: Edid,
        modes: Vec<ModeSpec>,
        primary: bool,
    ) {
        self.outputs.insert(
            name.clone(),
            Output {
                name: name.clone(),
                x: 0,
                y: 0,
                mode,
                modes,
                phys_mm,
                edid,
                primary,
                enabled: true,
                ws: 0,
                prev_ws: 0,
                scale: 1.0,
                policy: self.policy,
            },
        );
        self.relayout();
    }

    /// Mark one connector primary — the flag drives the origin
    /// placement contract, fullscreen fallbacks and X11 RandR
    /// primary. Re-runs the auto side-by-side layout, so call it
    /// at startup or accept that manual positions reset.
    pub fn set_primary(&mut self, name: &str) {
        if !self.outputs.contains_key(name) {
            tracing::warn!(output = %name, "set_primary: unknown output");
            return;
        }
        for o in self.outputs.values_mut() {
            o.primary = o.name == name;
        }
        self.relayout();
    }

    /// Primary contract: the primary output sits at the space
    /// origin; non-primary outputs tile rightward. Live-safe —
    /// callers may re-run this on any change.
    pub fn relayout(&mut self) {
        // A session with no enabled primary still needs one —
        // promote the first enabled output so the space never
        // collapses to overlapping origins (hot-unplug of the
        // primary, disable of the last primary via config).
        if !self.outputs.values().any(|o| o.primary && o.enabled) {
            if let Some(first) = self.outputs.values_mut().find(|o| o.enabled) {
                first.primary = true;
            }
        }
        // Partition: primary first at 0,0; the rest sorted by name.
        let mut others: Vec<String> = self
            .outputs
            .values()
            .filter(|o| !o.primary && o.enabled)
            .map(|o| o.name.clone())
            .collect();
        others.sort();
        let mut cursor = 0;
        if let Some(primary) = self.outputs.values_mut().find(|o| o.primary && o.enabled) {
            primary.x = 0;
            primary.y = 0;
            cursor = primary.mode.0 as i32;
            info!(name = %primary.name, "primary output at origin");
        }
        for name in others {
            if let Some(out) = self.outputs.get_mut(&name) {
                out.x = cursor;
                out.y = 0;
                cursor += out.mode.0 as i32;
            }
        }
    }

    /// The output currently showing workspace `n`, if any.
    pub fn output_showing(&self, ws: u8) -> Option<&Output> {
        self.outputs.values().find(|o| o.enabled && o.ws == ws)
    }

    /// Global-space rectangle of the primary output — fallback
    /// target for windows whose center sits off every screen.
    pub fn primary_rect(&self) -> (i32, i32, u32, u32) {
        self.outputs
            .values()
            .find(|o| o.primary && o.enabled)
            .or_else(|| self.outputs.values().find(|o| o.enabled))
            .map(|o| (o.x, o.y, o.mode.0, o.mode.1))
            .unwrap_or((0, 0, 1, 1))
    }

    /// Bounding box of the whole output layout — the space the
    /// pointer may roam and windows may occupy.
    pub fn bounds(&self) -> (f64, f64) {
        let mut w = 0i32;
        let mut h = 0i32;
        for o in self.outputs.values().filter(|o| o.enabled) {
            w = w.max(o.x + o.mode.0 as i32);
            h = h.max(o.y + o.mode.1 as i32);
        }
        (w.max(1) as f64, h.max(1) as f64)
    }

    /// Change the DEFAULT policy — applies to outputs added from
    /// now on; existing monitors keep their own `Output.policy`
    /// (the user's per-monitor choice is never overridden here).
    pub fn set_policy(&mut self, policy: LayoutPolicy) {
        if self.policy != policy {
            info!(from = ?self.policy, to = ?policy, "default layout policy switch");
            self.policy = policy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Master-stack contract: one window fills its output's whole
    /// usable rect — per-output tiling means the rect carries the
    /// output's origin, so a tile on the second monitor must sit
    /// inside the second monitor's global-space rect.
    #[test]
    fn tiles_one_window_fills_rect() {
        let t = tiles((1920, 0, 1280, 720), 1);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].x, t[0].y, t[0].w, t[0].h), (1920, 0, 1280, 720));
    }

    /// Two windows split master|stack; with n>2 the right column
    /// stacks vertically and every tile stays inside the rect —
    /// the "tiles never cross monitor boundaries" guarantee.
    #[test]
    fn tiles_master_stack_stays_inside_rect() {
        let rect = (0, 0, 1920, 1080);
        for n in [2usize, 3, 5] {
            let t = tiles(rect, n);
            assert_eq!(t.len(), n);
            assert_eq!(t[0].w, 960, "master owns the left half");
            for tile in &t {
                assert!(tile.x >= rect.0);
                assert!(tile.y >= rect.1);
                assert!(tile.x + tile.w as i32 <= rect.0 + rect.2 as i32);
                assert!(tile.y + tile.h as i32 <= rect.1 + rect.3 as i32);
            }
            // Stack tiles partition the right half exactly —
            // no gaps, no overlaps.
            let stack_h: u32 = t[1..].iter().map(|s| s.h).sum();
            assert_eq!(stack_h, rect.3);
        }
    }

    /// The default is Floating everywhere — the user's tiling
    /// choice is opt-in per output, never the baseline.
    #[test]
    fn policy_defaults_to_floating() {
        assert_eq!(Layout::new().policy, LayoutPolicy::Floating);
        assert_eq!(LayoutPolicy::Floating.toggle(), LayoutPolicy::Tiling);
    }
}
