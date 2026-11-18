// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_output_manager_v1 — live monitor configuration for
//! tools like wlr-randr, wdisplays and kanshi.
//!
//! The manager replays one `zwlr_output_head_v1` object per
//! layout output on bind — heads describe name, mode, position,
//! transform, scale and the enabled flag, each event gated to
//! the version the client bound. `create_configuration` returns
//! a transaction object (see `output_config.rs`); applying it
//! mutates `layout.outputs`, re-syncs every bound head and every
//! per-output wl_output, then bumps the configuration serial.

use std::sync::{Arc, Mutex};
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, zwlr_output_head_v1 as omh,
    zwlr_output_head_v1::ZwlrOutputHeadV1, zwlr_output_manager_v1 as omm,
    zwlr_output_manager_v1::ZwlrOutputManagerV1, zwlr_output_mode_v1 as ommode,
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};
use wayland_server::protocol::wl_output;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1 as ZwlrHead;
pub use wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1 as ZwlrOutputManager;

/// One live head object — binds a manager client's view to the
/// layout output it describes.
pub struct OmHead {
    pub obj: ZwlrOutputHeadV1,
    /// Connector name this head describes.
    pub output: String,
}

/// A validated output change queued for the backend — the
/// protocol layer commits it to the layout, the event loop
/// applies it to the DRM outputs (real re-modeset, viewport
/// move, CRTC blank) and reconciles on failure.
#[derive(Debug)]
pub enum OutOp {
    /// Re-modeset the connector to an advertised mode.
    Mode {
        name: String,
        w: u32,
        h: u32,
        hz: u32,
    },
    /// Move the output's viewport origin in global space.
    Position { name: String, x: i32, y: i32 },
    /// Enable or disable the connector's presentation.
    Enabled { name: String, on: bool },
}

/// Pending per-head changes inside an open configuration —
/// the cfg_head object writes into this, apply() reads it.
#[derive(Default)]
pub struct PendingOut {
    pub enabled: bool,
    /// (w, h, mHz) — set_mode or set_custom_mode.
    pub mode: Option<(u32, u32, u32)>,
    pub pos: Option<(i32, i32)>,
    pub transform: Option<u32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

/// Open transaction state — the configuration object holds the
/// serial the client based its changes on plus the per-head
/// pending records its cfg_head children are filling.
pub struct CfgData {
    pub base_serial: u32,
    /// head name → shared pending record (cfg_head udata).
    pub pending: Mutex<std::collections::BTreeMap<String, Arc<Mutex<PendingOut>>>>,
}

impl CompState {
    /// Create one mode resource for a head burst — the client
    /// matches it against the modes it can later pass to
    /// set_mode; udata carries (w, h, mHz) like every mode we
    /// advertise.
    fn om_mode_obj(
        &self,
        head: &ZwlrOutputHeadV1,
        v: u32,
        m: crate::wm::ModeSpec,
    ) -> Option<ZwlrOutputModeV1> {
        let client = head.client()?;
        client
            .create_resource::<ZwlrOutputModeV1, (u32, u32, u32), CompState>(
                &self.dh,
                v.min(3),
                (m.w, m.h, m.hz * 1000),
            )
            .ok()
    }

    /// The descriptor burst a freshly created head must send —
    /// every field gated to the client's bound version. All
    /// advertised connector modes go out as mode objects so
    /// set_mode has real choices, not only the current mode.
    pub(crate) fn om_head_describe(&self, head: &ZwlrOutputHeadV1, name: &str) {
        let v = head.version();
        let Some(out) = self.layout.outputs.get(name) else {
            return;
        };
        head.name(name.to_string());
        head.description(if out.edid.make.is_empty() && out.edid.model.is_empty() {
            format!("Odin output {name}")
        } else {
            format!("{} {} ({name})", out.edid.make, out.edid.model)
        });
        // Same physical dimensions wl_output.geometry advertises —
        // one source of truth per output (EDID-reported mm).
        head.physical_size(out.phys_mm.0 as i32, out.phys_mm.1 as i32);
        head.enabled(if out.enabled { 1 } else { 0 });
        // head.mode announces each object to the client — only
        // then may current_mode reference it.
        let mut current = None;
        for m in &out.modes {
            let Some(mode) = self.om_mode_obj(head, v, *m) else {
                continue;
            };
            head.mode(&mode);
            mode.size(m.w as i32, m.h as i32);
            mode.refresh((m.hz * 1000) as i32);
            if m.preferred {
                mode.preferred();
            }
            if (m.w, m.h, m.hz) == out.mode {
                current = Some(mode);
            }
        }
        // Fallback for outputs with no scanned mode list —
        // advertise at least the live mode so current_mode is
        // never empty.
        if current.is_none() {
            if let Some(mode) = self.om_mode_obj(
                head,
                v,
                crate::wm::ModeSpec {
                    w: out.mode.0,
                    h: out.mode.1,
                    hz: out.mode.2,
                    preferred: true,
                },
            ) {
                head.mode(&mode);
                mode.size(out.mode.0 as i32, out.mode.1 as i32);
                mode.refresh((out.mode.2 * 1000) as i32);
                mode.preferred();
                current = Some(mode);
            }
        }
        if let Some(mode) = current {
            head.current_mode(&mode);
        }
        head.position(out.x, out.y);
        head.transform(wl_output::Transform::Normal);
        head.scale(out.scale);
        // v2 fields — real EDID identity now; kanshi-style
        // profile matching keys off make/model/serial.
        if v >= 2 {
            head.make(out.edid.make.clone());
            head.model(out.edid.model.clone());
            head.serial_number(out.edid.serial.clone());
        }
        if v >= 4 {
            head.adaptive_sync(omh::AdaptiveSyncState::Disabled);
        }
    }

    /// A manager bound — replay every layout output as a head,
    /// then the done serial anchors this snapshot.
    pub(crate) fn om_replay(&mut self, mgr: &ZwlrOutputManagerV1) {
        let names: Vec<String> = self.layout.outputs.keys().cloned().collect();
        for name in names {
            let Some(client) = mgr.client() else { continue };
            let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, String, CompState>(
                &self.dh,
                mgr.version().min(4),
                name.clone(),
            ) else {
                continue;
            };
            mgr.head(&head);
            self.om_head_describe(&head, &name);
            self.om_heads.push(OmHead {
                obj: head,
                output: name,
            });
        }
        mgr.done(self.om_serial);
    }

    /// Layout changed — re-send the changed properties on every
    /// live head, then each manager's done carries the new serial.
    pub(crate) fn om_sync(&mut self) {
        self.om_serial = self.om_serial.wrapping_add(1);
        for h in &self.om_heads {
            if !h.obj.is_alive() {
                continue;
            }
            let v = h.obj.version();
            if let Some(out) = self.layout.outputs.get(&h.output) {
                h.obj.enabled(if out.enabled { 1 } else { 0 });
                h.obj.position(out.x, out.y);
                h.obj.scale(out.scale);
                // A mode change needs a fresh mode object the
                // current_mode event can point at.
                if let Some(mode) = self.om_mode_obj(
                    &h.obj,
                    v,
                    crate::wm::ModeSpec {
                        w: out.mode.0,
                        h: out.mode.1,
                        hz: out.mode.2,
                        preferred: false,
                    },
                ) {
                    h.obj.mode(&mode);
                    mode.size(out.mode.0 as i32, out.mode.1 as i32);
                    mode.refresh((out.mode.2 * 1000) as i32);
                    h.obj.current_mode(&mode);
                }
            }
        }
        for m in &self.om_managers {
            if m.is_alive() {
                m.done(self.om_serial);
            }
        }
    }

    /// Per-output wl_output resources mirror geometry/mode after
    /// an applied configuration — same contract as bind-time.
    pub(crate) fn om_sync_wl_outputs(&self) {
        for (res, name) in &self.outputs {
            if !res.is_alive() {
                continue;
            }
            let Some(out) = self.layout.outputs.get(name) else {
                continue;
            };
            res.geometry(
                out.x,
                out.y,
                out.phys_mm.0 as i32,
                out.phys_mm.1 as i32,
                wl_output::Subpixel::Unknown,
                if out.edid.make.is_empty() {
                    "unknown".to_string()
                } else {
                    out.edid.make.clone()
                },
                if out.edid.model.is_empty() {
                    name.clone()
                } else {
                    out.edid.model.clone()
                },
                wl_output::Transform::Normal,
            );
            res.mode(
                wl_output::Mode::Current | wl_output::Mode::Preferred,
                out.mode.0 as i32,
                out.mode.1 as i32,
                (out.mode.2 * 1000) as i32,
            );
            res.done();
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        mgr: &ZwlrOutputManagerV1,
        request: omm::Request,
        _: &(),
        _: &DisplayHandle,
        dinit: &mut DataInit<'_, Self>,
    ) {
        match request {
            omm::Request::CreateConfiguration { id, serial } => {
                // The serial the client based its changes on —
                // stale on apply → cancelled, per protocol.
                dinit.init::<ZwlrOutputConfigurationV1, CfgData>(
                    id,
                    CfgData {
                        base_serial: serial,
                        pending: Mutex::new(Default::default()),
                    },
                );
            }
            omm::Request::Stop => {
                mgr.finished();
                let cid = mgr.client().map(|c| c.id());
                state.om_managers.retain(|m| m.id() != mgr.id());
                state
                    .om_heads
                    .retain(|h| h.obj.client().map(|c| c.id()) != cid);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, String> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        head: &ZwlrOutputHeadV1,
        request: omh::Request,
        _: &String,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let omh::Request::Release = request {
            state.om_heads.retain(|h| h.obj.id() != head.id());
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, (u32, u32, u32)> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputModeV1,
        _: ommode::Request,
        _: &(u32, u32, u32),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // release only — nothing to track on our side.
    }
}
