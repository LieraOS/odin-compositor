// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! DRM/KMS device discovery — the ground truth for outputs.
//!
//! Phase 1 scope: open each GPU the session hands us, enumerate
//! connectors and their modes, report what is plugged in.
//! Rendering/atomic commits land in the renderer milestone on top
//! of this scan — the data model is already the one wl_output
//! objects will be built from.

use anyhow::{Context, Result};
use drm::control::{connector::Info as ConnectorInfo, connector::Interface, Device as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use tracing::info;

/// One display connector with its negotiated mode list.
pub struct ConnectorScan {
    pub card: PathBuf,
    /// DRM connector handle (kept for the future modeset path).
    #[allow(dead_code)]
    pub connector_id: u32,
    /// e.g. "DP-3", "HDMI-A-1" — same naming wl_output uses.
    pub name: String,
    pub connected: bool,
    /// Preferred (usually native) mode, if any.
    pub preferred_mode: Option<String>,
    /// Preferred mode as (w, h, Hz) — the layout needs real widths
    /// to place outputs side by side in global space.
    pub mode: (u32, u32, u32),
    /// Every mode the connector advertises — wl_output and
    /// zwlr_output_head_v1 expose the full list so clients can
    /// pick non-preferred modes.
    pub modes: Vec<crate::wm::ModeSpec>,
    /// Monitor identity decoded from the EDID property blob.
    pub edid: crate::wm::Edid,
    pub physical_mm: (u32, u32),
}

/// Fetch the connector's EDID property blob — the kernel stores
/// the monitor's identification block as a blob-valued connector
/// property; the value is the blob id, `get_property_blob`
/// resolves it to the raw 128-byte (plus extensions) block.
fn read_edid(card: &Card, conn: &ConnectorInfo) -> Vec<u8> {
    let Ok(set) = card.get_properties(conn.handle()) else {
        return Vec::new();
    };
    for (h, v) in &set {
        let Ok(info) = card.get_property(*h) else {
            continue;
        };
        if info.name().to_string_lossy() == "EDID" && *v != 0 {
            return card.get_property_blob(*v).unwrap_or_default();
        }
    }
    Vec::new()
}

/// Decode the EDID base block into a monitor identity.
/// Layout: bytes 8-9 manufacturer PNP id (packed 5-bit ASCII),
/// 10-11 product code LE, 12-15 serial LE; descriptor blocks at
/// offset 54 step 18 — tag 0xFC carries the monitor name string,
/// 0xFF an ASCII serial. Empty fields stay empty — a monitor
/// that reports no serial has no serial.
fn parse_edid(raw: &[u8]) -> crate::wm::Edid {
    let mut e = crate::wm::Edid::default();
    if raw.len() < 128 {
        return e;
    }
    // Manufacturer id: 3 letters packed into two big-endian bytes.
    let m = u16::from_be_bytes([raw[8], raw[9]]);
    let letter = |shift: u16| char::from_u32((((m >> shift) & 0x1f) as u32) + 64).unwrap_or(' ');
    e.make = [letter(10), letter(5), letter(0)]
        .into_iter()
        .collect::<String>()
        .trim()
        .to_string();
    let product = u16::from_le_bytes([raw[10], raw[11]]);
    let serial_num = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
    // Display descriptors: 18-byte blocks, [0..3] zero tag header.
    let desc_string = |d: &[u8]| -> String {
        // Content runs 5..18, newline-terminated, space-padded.
        let s = String::from_utf8_lossy(&d[5..18]).into_owned();
        s.split('\n').next().unwrap_or("").trim().to_string()
    };
    for i in (54..126).step_by(18) {
        let d = &raw[i..i + 18];
        match d[3] {
            0xFC if d[0..3] == [0, 0, 0] => e.model = desc_string(d),
            0xFF if d[0..3] == [0, 0, 0] => {
                e.serial = desc_string(d);
            }
            _ => {}
        }
    }
    // The 0xFF serial STRING wins over the numeric field — that's
    // the serial clients and display-profile matching key on.
    // The raw number is the fallback for monitors without
    // a serial descriptor.
    if e.serial.is_empty() && serial_num != 0 {
        e.serial = serial_num.to_string();
    }
    // No name descriptor — the product code is the honest model.
    if e.model.is_empty() {
        e.model = format!("0x{product:04x}");
    }
    e
}

/// One discovered DRM card.
pub struct Scan {
    pub connectors: Vec<ConnectorScan>,
    pub cards: Vec<PathBuf>,
}

/// Adapter making a session-obtained device fd a `drm::Device`.
struct Card(std::os::fd::OwnedFd);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// Name the connector the way the rest of the stack expects it
/// ("DP-3" style — kernel connector type + id). Shared with the
/// output layer so scan and modeset agree on names.
pub fn connector_name(info: &ConnectorInfo, id: u32) -> String {
    let ty = match info.interface() {
        Interface::DisplayPort => "DP",
        Interface::HDMIA => "HDMI-A",
        Interface::HDMIB => "HDMI-B",
        Interface::DVII => "DVI-I",
        Interface::DVID => "DVI-D",
        Interface::DVIA => "DVI-A",
        Interface::VGA => "VGA",
        Interface::LVDS => "LVDS",
        Interface::EmbeddedDisplayPort => "eDP",
        Interface::Virtual => "Virtual",
        _ => "Unknown",
    };
    format!("{ty}-{id}")
}

/// dev_t of the first primary DRM node — announced to clients
/// via the dmabuf v4 `main_device` event so EGL stacks
/// (XWayland glamor, vaapi) open the same GPU the compositor
/// uses. Purely stat-based — works on every backend including
/// nested, where no session fd exists. 0 when no DRM is there.
pub fn primary_devt() -> u64 {
    use std::os::unix::fs::MetadataExt;
    (0..8)
        .map(|n| PathBuf::from(format!("/dev/dri/card{n}")))
        .find_map(|p| std::fs::metadata(&p).ok().map(|m| m.rdev()))
        .unwrap_or(0)
}

/// The render node paired with a primary card — syncobj ioctls
/// live there (some drivers reject them on primary fds). Found
/// through sysfs: /sys/class/drm/<card>/device/drm/renderD*.
pub fn render_node_for(card: &std::path::Path) -> Option<PathBuf> {
    let card = card.file_name()?.to_str()?;
    std::fs::read_dir(format!("/sys/class/drm/{card}/device/drm"))
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("renderD"))
        .map(|n| PathBuf::from(format!("/dev/dri/{n}")))
}

/// Enumerate every /dev/dri/cardN node and probe its connectors.
pub fn scan(session: &mut crate::session::Session) -> Result<Scan> {
    let mut cards = Vec::new();
    let mut connectors = Vec::new();
    for n in 0..8 {
        let path = PathBuf::from(format!("/dev/dri/card{n}"));
        if !path.exists() {
            continue;
        }
        let device = match session.open_device(&path) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?path, ?err, "skipping card");
                continue;
            }
        };
        let card = Card(device);
        // Hardware contract: probe the driver and pick the render
        // path *before* the device list is trusted — Intel/AMD/
        // NVIDIA share GBM+EGL, unknowns fall back to software.
        let gpu = super::gpu::probe(&path);
        super::gpu::render_path(gpu);
        let handles = card
            .resource_handles()
            .with_context(|| format!("resource_handles on {path:?}"))?;
        for &conn_id in handles.connectors() {
            let info = match card.get_connector(conn_id, true) {
                Ok(i) => i,
                Err(err) => {
                    tracing::warn!(?conn_id, ?err, "connector probe failed");
                    continue;
                }
            };
            let connected = matches!(info.state(), drm::control::connector::State::Connected);
            let preferred = info
                .modes()
                .iter()
                .find(|m| {
                    m.mode_type()
                        .contains(drm::control::ModeTypeFlags::PREFERRED)
                })
                .or_else(|| info.modes().first());
            let modes: Vec<crate::wm::ModeSpec> = info
                .modes()
                .iter()
                .map(|m| crate::wm::ModeSpec {
                    w: m.size().0 as u32,
                    h: m.size().1 as u32,
                    hz: m.vrefresh(),
                    preferred: m
                        .mode_type()
                        .contains(drm::control::ModeTypeFlags::PREFERRED),
                })
                .collect();
            connectors.push(ConnectorScan {
                card: path.clone(),
                connector_id: u32::from(conn_id),
                name: connector_name(&info, info.interface_id()),
                connected,
                preferred_mode: preferred
                    .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh())),
                mode: preferred
                    .map(|m| (m.size().0 as u32, m.size().1 as u32, m.vrefresh()))
                    .unwrap_or((0, 0, 0)),
                modes,
                edid: parse_edid(&read_edid(&card, &info)),
                physical_mm: info.size().unwrap_or((0, 0)),
            });
        }
        cards.push(path);
    }
    Ok(Scan { connectors, cards })
}

/// Read-only atomic capability probe — enumerates the object
/// properties every atomic-KMS feature needs (VRR toggle, HDR
/// metadata, sync fences, LUT sizes) and logs one line per
/// output. Pure diagnostics — the groundwork for the atomic
/// commit path; it changes nothing about the legacy flips.
pub fn probe_atomic(
    card: &impl drm::control::Device,
    conn: &drm::control::connector::Info,
    crtc: drm::control::crtc::Handle,
    name: &str,
) {
    /// Connector-level knobs — VRR/HDR/EDID live here.
    const CONN_PROPS: &[&str] = &[
        "VRR_ENABLED",
        "HDR_OUTPUT_METADATA",
        "Colorspace",
        "non-desktop",
        "DPMS",
        "CRTC_ID",
        "link-status",
        "max bpc",
        "Broadcast RGB",
    ];
    /// CRTC-level knobs — mode blob + fence/LUT plumbing for
    /// atomic commits.
    const CRTC_PROPS: &[&str] = &[
        "MODE_ID",
        "ACTIVE",
        "OUT_FENCE_PTR",
        "GAMMA_LUT_SIZE",
        "DEGAMMA_LUT_SIZE",
        "CTM",
        "GAMMA_LUT",
    ];
    let mut found: Vec<String> = Vec::new();
    if let Ok(set) = card.get_properties(conn.handle()) {
        for (h, v) in &set {
            let Ok(info) = card.get_property(*h) else {
                continue;
            };
            let n = info.name().to_string_lossy().into_owned();
            if CONN_PROPS.iter().any(|i| n == *i) {
                found.push(format!("{n}={v}"));
            }
        }
    }
    if let Ok(set) = card.get_properties(crtc) {
        for (h, v) in &set {
            let Ok(info) = card.get_property(*h) else {
                continue;
            };
            let n = info.name().to_string_lossy().into_owned();
            if CRTC_PROPS.iter().any(|i| n == *i) {
                found.push(format!("crtc:{n}={v}"));
            }
        }
    }
    tracing::info!(output = %name, "atomic caps: {}", found.join(" "));
}

/// Human-readable dump of the scan — the phase-1 proof output.
pub fn report(scan: &Scan) {
    info!(cards = ?scan.cards, "drm devices discovered");
    for c in &scan.connectors {
        info!(
            connector = %c.name,
            card = %c.card.display(),
            connected = c.connected,
            modes = c.modes.len(),
            preferred = ?c.preferred_mode,
            phys_mm = ?c.physical_mm,
            make = %c.edid.make,
            model = %c.edid.model,
            serial = %c.edid.serial,
            "connector"
        );
    }
}
