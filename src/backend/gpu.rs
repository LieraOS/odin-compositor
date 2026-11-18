// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! GPU capability probe — decides per-device how buffers and
//! rendering are driven. Hardware support is a first-class
//! contract, not an afterthought.
//!
//! Coverage model:
//! - Intel/AMD: GBM + EGL/GLES direct (standard KMS path)
//! - NVIDIA ≥ 495: GBM + EGL/GLES direct (same path; the driver
//!   exposes GBM natively). Older drivers get the EGLStreams or
//!   legacy-path fallback slot — a dedicated quirk module, never
//!   architecture-bending branches.
//! - Anything else: llvmpipe/softpipe fallback via GBM — ugly but
//!   bootable, logged loudly.

use std::path::Path;
use tracing::{info, warn};

/// What the probe found for one GPU node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Intel,
    Amd,
    Nvidia,
    Other,
}

/// Rendering strategy selected for the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderPath {
    /// GBM buffers + EGL/GLES — the standard path.
    GbmEgl,
    /// Software rasterizer fallback (llvmpipe).
    Software,
}

/// Probe one DRM card: read the PCI/driver identity and pick the
/// render path. Real driver detection goes through udev/sysfs
/// (`/sys/class/drm/cardN/device/driver`), not vendor guesses.
pub fn probe(card: &Path) -> GpuKind {
    let name = card.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let driver_link = std::fs::read_link(format!("/sys/class/drm/{name}/device/driver")).ok();
    let driver = driver_link
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let kind = match driver.as_str() {
        d if d.contains("i915") => GpuKind::Intel,
        d if d.contains("amdgpu") || d.contains("radeon") => GpuKind::Amd,
        d if d.contains("nvidia") || d.contains("nouveau") => GpuKind::Nvidia,
        _ => GpuKind::Other,
    };
    info!(?card, %driver, ?kind, "gpu probed");
    kind
}

/// Render path for the probed device. NVIDIA stays on the same
/// GBM+EGL route as everyone else — only logged separately so
/// quirks are diagnosable from the log instead of hidden.
pub fn render_path(kind: GpuKind) -> RenderPath {
    match kind {
        GpuKind::Nvidia => {
            info!("nvidia gpu — GBM+EGL path (495+ drivers)");
            RenderPath::GbmEgl
        }
        GpuKind::Other => {
            warn!("unrecognized gpu — falling back to software rasterizer");
            RenderPath::Software
        }
        _ => RenderPath::GbmEgl,
    }
}
