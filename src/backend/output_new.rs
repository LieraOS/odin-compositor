// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `Output::new` — open the card through the session, find the
//! named connector, pick a CRTC + preferred mode. Split from
//! render.rs to keep both files inside the size budget.

use anyhow::{Context, Result};
use drm::control::Device as ControlDevice;
use std::os::fd::{AsFd, AsRawFd};
use tracing::info;

use super::render::{set_master, Card, Output};

impl Output {
    /// Open the card through the session, find the named connector,
    /// pick a CRTC + preferred mode. `used` is the set of CRTCs
    /// already claimed by other outputs — two connectors can never
    /// share one. This is the udev-path output the frame source
    /// drives; scanout uses a dumb buffer, so no GBM device is
    /// needed here.
    pub fn new(
        session: &mut crate::session::Session,
        card_path: &std::path::Path,
        conn_name: &str,
        origin: (i32, i32),
        used: &mut std::collections::HashSet<drm::control::crtc::Handle>,
    ) -> Result<Self> {
        let device = session.open_device(card_path)?;
        let card = Card(device);
        // The seat may have handed us the fd while the previous
        // display server still held DRM master — claim it now that
        // the VT switch revoked theirs. EINVAL inside set_master
        // means "already master" and is fine.
        match set_master(card.as_fd()) {
            Ok(()) => info!("drm: master acquired on {}", card_path.display()),
            Err(err) => tracing::warn!(?err, "drmSetMaster — will retry on EACCES"),
        }
        let handles = card.resource_handles()?;
        let mut found = None;
        for &conn_id in handles.connectors() {
            let conn = card.get_connector(conn_id, true)?;
            let full = super::drm::connector_name(&conn, conn.interface_id());
            if full == conn_name
                && matches!(conn.state(), drm::control::connector::State::Connected)
            {
                found = Some(conn);
                break;
            }
        }
        let conn = found.context("connector not found/connected")?;
        // Driver name → dirtyfb policy: virtio needs the explicit
        // flush; real drivers get coherent dumb buffers without it.
        let card_id = card_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let driver = std::fs::read_link(format!("/sys/class/drm/{card_id}/device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let needs_dirtyfb = driver.contains("virtio");
        let mode = conn
            .modes()
            .iter()
            .find(|m| {
                m.mode_type()
                    .contains(drm::control::ModeTypeFlags::PREFERRED)
            })
            .or_else(|| conn.modes().first())
            .cloned()
            .context("no modes on connector")?;
        // CRTC selection per connector: prefer the CRTC it is
        // already driven by (avoids stealing the sibling output's
        // pipe), else any CRTC a candidate encoder allows that no
        // other output has claimed.
        let current = conn
            .current_encoder()
            .and_then(|enc| card.get_encoder(enc).ok())
            .and_then(|enc| enc.crtc())
            .filter(|c| !used.contains(c));
        let crtc = current
            .or_else(|| {
                conn.encoders()
                    .iter()
                    .filter_map(|&e| card.get_encoder(e).ok())
                    .flat_map(|e| handles.filter_crtcs(e.possible_crtcs()))
                    .find(|c| !used.contains(c))
            })
            .or_else(|| handles.crtcs().iter().find(|c| !used.contains(c)).copied())
            .context("no unused crtc for connector")?;
        used.insert(crtc);
        info!(
            conn = conn_name,
            w = mode.size().0,
            h = mode.size().1,
            driver = driver,
            dirtyfb = needs_dirtyfb,
            x = origin.0,
            y = origin.1,
            "udev output ready"
        );
        let (mw, mh) = mode.size();
        let backend = crate::renderer::create(
            card.as_fd().as_raw_fd(),
            crate::renderer::OutputSpec {
                w: mw as u32,
                h: mh as u32,
                ox: origin.0,
                oy: origin.1,
                name: conn_name.to_string(),
            },
        );
        // Atomic-KMS capability probe — reads what VRR/HDR/fence
        // plumbing the driver exposes; groundwork for the atomic
        // commit path, logged once per output.
        super::drm::probe_atomic(&card, &conn, crtc, conn_name);
        Ok(Self {
            card,
            conn,
            name: conn_name.to_string(),
            crtc,
            mode,
            origin,
            dumb: None,
            fb: None,
            last_fb: None,
            crtc_set: false,
            backend,
            needs_dirtyfb,
            master_denials: 0,
            fatal: false,
            render_errors: 0,
            pending_damage: std::collections::VecDeque::new(),
            damage_seq: 0,
            staged: None,
            map_ptr: std::ptr::null_mut(),
            map_pitch: 0,
            hwcursor: super::hwcursor::HwCursor::new(),
            scan_fbs: Vec::new(),
            scan_tried: false,
            scan_dbs: Vec::new(),
            scan_maps: Vec::new(),
            flipped_parity: None,
            direct_fbs: Vec::new(),
            direct_buf: None,
            direct_dead: false,
            dump_done: 0,
        })
    }
}
