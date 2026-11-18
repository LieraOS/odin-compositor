// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Output-configuration transactions — the apply/test half of
//! zwlr_output_manager_v1 (head/mode plumbing lives in
//! `output_mgmt.rs`).
//!
//! `enable_head` spawns a cfg_head child sharing a pending
//! record; its set_* requests write into it. `apply` validates
//! (serial fresh, at least one output stays enabled), mutates
//! `layout.outputs`, resizes the pointer space, re-tiles and
//! pushes the new state to every bound head and wl_output.

use std::sync::{Arc, Mutex};
use tracing::info;
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1 as omch,
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1 as omc, zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
    zwlr_output_head_v1 as omh,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::output_mgmt::{CfgData, OutOp, PendingOut};
use super::CompState;
use wayland_server::protocol::wl_output;

/// Fold the pending records into the enabled set — pure
/// validation shared by apply and test. Err when the config
/// leaves the session with no output at all, asks for a mode the
/// connector never advertised, or requests features we cannot
/// honour (transform, adaptive sync) — a `failed` reply is the
/// honest answer, a silent ignore is not.
fn validate(state: &CompState, cfg: &CfgData) -> Result<(), ()> {
    let pending = cfg.pending.lock().unwrap();
    let mut enabled = state
        .layout
        .outputs
        .values()
        .filter(|o| o.enabled)
        .map(|o| o.name.clone())
        .collect::<Vec<_>>();
    for (name, rec) in pending.iter() {
        let rec = rec.lock().unwrap();
        let Some(out) = state.layout.outputs.get(name) else {
            return Err(()); // pending head for an output we lack
        };
        if let Some((w, h, mhz)) = rec.mode {
            // The client may only pick modes the connector
            // advertised — anything else cannot be driven.
            let known = out
                .modes
                .iter()
                .any(|m| m.w == w && m.h == h && m.hz * 1000 == mhz);
            if !known {
                return Err(());
            }
        }
        // Transforms need per-output buffer transforms we don't
        // render yet — reject rather than lie about applied.
        if let Some(t) = rec.transform {
            if t != u32::from(wl_output::Transform::Normal) {
                return Err(());
            }
        }
        // VRR plumbing is probed but not wired to commits —
        // same honesty: refused, not silently dropped.
        if rec.adaptive_sync == Some(true) {
            return Err(());
        }
        match (rec.enabled, enabled.iter().any(|n| n == name)) {
            (false, true) => enabled.retain(|n| n != name),
            (true, false) => enabled.push(name.clone()),
            _ => {}
        }
    }
    if enabled.is_empty() {
        return Err(()); // zero outputs — no session left
    }
    Ok(())
}

/// Write the validated pending records into the layout and queue
/// the matching hardware ops for the backend — the event loop
/// drains `pending_outops` and executes the real re-modeset,
/// viewport move or CRTC blank. Then propagate: pointer space,
/// tiling, head re-sync, wl_output re-sync.
fn commit(state: &mut CompState, cfg: &CfgData) {
    for (name, rec) in cfg.pending.lock().unwrap().iter() {
        let rec = rec.lock().unwrap();
        let Some(out) = state.layout.outputs.get_mut(name) else {
            continue;
        };
        if out.enabled != rec.enabled {
            out.enabled = rec.enabled;
            state.pending_outops.push(OutOp::Enabled {
                name: name.clone(),
                on: rec.enabled,
            });
        }
        if let Some((x, y)) = rec.pos {
            if (out.x, out.y) != (x, y) {
                out.x = x;
                out.y = y;
                state.pending_outops.push(OutOp::Position {
                    name: name.clone(),
                    x,
                    y,
                });
            }
        }
        if let Some((w, h, mhz)) = rec.mode {
            let hz = mhz / 1000;
            if out.mode != (w, h, hz) {
                out.mode = (w, h, hz);
                state.pending_outops.push(OutOp::Mode {
                    name: name.clone(),
                    w,
                    h,
                    hz,
                });
            }
        }
        if let Some(s) = rec.scale {
            out.scale = s;
        }
        info!(output = %name, enabled = rec.enabled,
              pos = ?rec.pos, mode = ?rec.mode,
              "output configuration applied");
    }
    // Everything sized off the output set updates — the bounding
    // box feeds pointer clamping and the tile area.
    state.space = state.layout.bounds();
    state.apply_layout();
    state.damage_everything();
    state.om_sync();
    state.om_sync_wl_outputs();
}

impl Dispatch<ZwlrOutputConfigurationV1, CfgData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        cfg_obj: &ZwlrOutputConfigurationV1,
        request: omc::Request,
        cfg: &CfgData,
        _: &DisplayHandle,
        dinit: &mut DataInit<'_, Self>,
    ) {
        match request {
            omc::Request::EnableHead { id, head } => {
                // The cfg_head child shares the pending record —
                // its set_* requests mutate it until apply reads.
                let name = head.data::<String>().cloned().unwrap_or_default();
                let rec = Arc::new(Mutex::new(PendingOut {
                    enabled: true,
                    ..Default::default()
                }));
                cfg.pending.lock().unwrap().insert(name, rec.clone());
                dinit.init::<ZwlrOutputConfigurationHeadV1, Arc<Mutex<PendingOut>>>(id, rec);
            }
            omc::Request::DisableHead { head } => {
                let name = head.data::<String>().cloned().unwrap_or_default();
                let rec = Arc::new(Mutex::new(PendingOut {
                    enabled: false,
                    ..Default::default()
                }));
                cfg.pending.lock().unwrap().insert(name, rec);
            }
            omc::Request::Apply => {
                if cfg.base_serial != state.om_serial {
                    cfg_obj.cancelled(); // layout moved under the client
                } else if validate(state, cfg).is_err() {
                    cfg_obj.failed();
                } else {
                    commit(state, cfg);
                    cfg_obj.succeeded();
                }
            }
            omc::Request::Test => {
                if cfg.base_serial != state.om_serial {
                    cfg_obj.cancelled();
                } else if validate(state, cfg).is_err() {
                    cfg_obj.failed();
                } else {
                    cfg_obj.succeeded();
                }
            }
            omc::Request::Destroy => {} // udata drops with the object
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, Arc<Mutex<PendingOut>>> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputConfigurationHeadV1,
        request: omch::Request,
        rec: &Arc<Mutex<PendingOut>>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut p = rec.lock().unwrap();
        match request {
            omch::Request::SetMode { mode } => {
                // The mode object carries (w,h,mHz) as udata —
                // every mode we advertise does.
                if let Some((w, h, mhz)) = mode.data::<(u32, u32, u32)>() {
                    p.mode = Some((*w, *h, *mhz));
                }
            }
            omch::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                p.mode = Some((width as u32, height as u32, refresh as u32));
            }
            omch::Request::SetPosition { x, y } => {
                p.pos = Some((x, y));
            }
            omch::Request::SetTransform {
                transform: WEnum::Value(t),
            } => {
                p.transform = Some(u32::from(t));
            }
            omch::Request::SetScale { scale } => {
                p.scale = Some(scale);
            }
            omch::Request::SetAdaptiveSync {
                state: WEnum::Value(s),
            } => {
                p.adaptive_sync = Some(s == omh::AdaptiveSyncState::Enabled);
            }
            _ => {}
        }
    }
}
