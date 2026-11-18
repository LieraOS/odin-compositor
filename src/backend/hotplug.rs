// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Monitor hotplug — a udev event on the drm subsystem marks the
//! flag; the next frame tick re-scans every card's connectors and
//! diffs them against the layout:
//!
//! * newly connected  → layout entry + DRM Output + wl_output
//!   global + output-management head announce;
//! * disconnected     → CRTC blanked, Output dropped, layout
//!   entry removed, wl_output global withdrawn, head `finished`;
//! * still connected  → metadata refresh only (EDID/mode list
//!   can change in place, e.g. after a link retrain).
//!
//! Positions of surviving outputs are preserved — the auto
//! side-by-side relayout only shapes the initial scan.

use drm::control::Device as ControlDevice;
use std::collections::HashSet;
use tracing::{info, warn};

use super::eventloop::CompState;

/// Re-scan all DRM cards and reconcile the layout + output set
/// with what is actually plugged in. Runs inside the frame tick —
/// a connector probe is a few ms of ioctls, acceptable for the
/// rare event.
pub fn rescan(state: &mut CompState) {
    let scan = match super::drm::scan(&mut state.session) {
        Ok(s) => s,
        Err(err) => {
            warn!(?err, "hotplug rescan failed — keeping current outputs");
            return;
        }
    };
    let live: HashSet<&str> = scan
        .connectors
        .iter()
        .filter(|c| c.connected)
        .map(|c| c.name.as_str())
        .collect();

    // --- unplugged outputs --------------------------------------
    let removed: Vec<String> = state
        .comp
        .layout
        .outputs
        .keys()
        .filter(|n| !live.contains(n.as_str()))
        .cloned()
        .collect();
    for name in removed {
        info!(output = %name, "monitor unplugged");
        // Blank the CRTC before the output drops — a connector
        // that comes back on a later plug must not inherit a
        // stale front buffer.
        if let Some(i) = state.outputs.iter().position(|o| o.name == name) {
            let mut out = state.outputs.remove(i);
            if out.crtc_set {
                let _ = out.card.set_crtc(out.crtc, None, (0, 0), &[], None);
            }
            // Imported client dmabuf framebuffers die with the
            // output — no orphaned GEM handles on this card.
            out.clear_direct_fbs();
        }
        let was_primary = state
            .comp
            .layout
            .outputs
            .get(&name)
            .map(|o| o.primary)
            .unwrap_or(false);
        state.comp.layout.outputs.remove(&name);
        if was_primary {
            // Keep the primary contract: promote the first
            // remaining enabled output.
            if let Some(first) = state.comp.layout.outputs.values_mut().find(|o| o.enabled) {
                first.primary = true;
            }
        }
        state.comp.output_removed(&name);
    }

    // --- plugged + metadata refresh ------------------------------
    let mut used: HashSet<drm::control::crtc::Handle> =
        state.outputs.iter().map(|o| o.crtc).collect();
    let mut meta_changed = false;
    for c in &scan.connectors {
        if !c.connected {
            continue;
        }
        if let Some(out) = state.comp.layout.outputs.get_mut(&c.name) {
            // Already known — only the advertised mode list, the
            // physical size or the monitor identity may have
            // changed underneath us.
            if out.modes != c.modes || out.phys_mm != c.physical_mm || out.edid != c.edid {
                out.modes = c.modes.clone();
                out.phys_mm = c.physical_mm;
                out.edid = c.edid.clone();
                meta_changed = true;
            }
            continue;
        }
        // New monitor — place it right of the current bounding
        // box instead of running the auto-relayout over the
        // user's configured positions.
        let x = state.comp.layout.bounds().0 as i32;
        state.comp.layout.outputs.insert(
            c.name.clone(),
            crate::wm::Output {
                name: c.name.clone(),
                x,
                y: 0,
                mode: c.mode,
                modes: c.modes.clone(),
                phys_mm: c.physical_mm,
                edid: c.edid.clone(),
                primary: false,
                enabled: true,
                ws: 0,
                prev_ws: 0,
                scale: 1.0,
                // Config chain for a fresh monitor: explicit
                // policy.<name>, else the `policy =` all-monitors
                // default, else the layout default (Floating).
                policy: state
                    .comp
                    .cfg
                    .out_policy
                    .get(&c.name)
                    .copied()
                    .or(state.comp.cfg.default_policy)
                    .unwrap_or(state.comp.layout.policy),
            },
        );
        match super::render::Output::new(&mut state.session, &c.card, &c.name, (x, 0), &mut used) {
            Ok(o) => {
                info!(output = %c.name, "monitor plugged in");
                state.outputs.push(o);
                state.comp.output_added(&c.name);
            }
            Err(err) => {
                // No usable CRTC/backend — an output that cannot
                // present must not exist for clients.
                warn!(output = %c.name, ?err, "hotplugged output init failed");
                state.comp.layout.outputs.remove(&c.name);
            }
        }
    }

    // Layout derived state follows every change.
    state.comp.space = state.comp.layout.bounds();
    state.comp.apply_layout();
    state.comp.damage_everything();
    if meta_changed {
        state.comp.om_sync();
        state.comp.om_sync_wl_outputs();
    }
    state.comp.dirty = true;
}
