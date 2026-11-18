// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_gamma_control_v1 — per-output gamma ramps for night
//! light clients (wlsunset, gammastep).
//!
//! The client hands us a table over an fd: `size` u16 entries
//! per channel (r, g, b in order). We advertise the classic
//! 256-entry ramp. On output paths the ramp applies as a LUT on
//! the composited frame — the same place a real CRTC would use
//! drmModeCrtcSetGamma — and intentionally NOT into screencopy
//! buffers (a capture sees pre-gamma pixels, like hardware).

use std::sync::Arc;
use tracing::info;
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1 as gcm, zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1 as gc, zwlr_gamma_control_v1::ZwlrGammaControlV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1 as ZwlrGammaControlManager;

/// Entries per channel — the classic 8-bit ramp every real CRTC
/// exposes; clients build their tables to this size.
pub const GAMMA_SIZE: usize = 256;

/// udata on the control object — the connector name it drives.
pub type GammaCtl = String;

impl CompState {
    /// The ramp a given output renders with, if any — the
    /// present path applies it after composite.
    pub fn gamma_lut(&self, output: &str) -> Option<Arc<Vec<u16>>> {
        self.gamma.get(output).map(|(_, r)| r.clone())
    }
}

/// Apply a 3×256 u16 ramp as a per-pixel LUT on a BGRA frame —
/// the software stand-in for the CRTC gamma stage. `clip`
/// (frame-local rects) restricts the pass to damaged regions:
/// retained pixels already carry last present's LUT.
pub fn apply_gamma(frame: &mut [u8], fw: u32, ramp: &[u16], clip: Option<&[(i32, i32, i32, i32)]>) {
    let (r, g, b) = (
        &ramp[..GAMMA_SIZE],
        &ramp[GAMMA_SIZE..2 * GAMMA_SIZE],
        &ramp[2 * GAMMA_SIZE..],
    );
    let mut apply = |px: &mut [u8]| {
        // BGRA byte order — the LUT scales each channel through
        // its u16 table entry and folds back to 8 bits.
        px[0] = (b[px[0] as usize] >> 8) as u8;
        px[1] = (g[px[1] as usize] >> 8) as u8;
        px[2] = (r[px[2] as usize] >> 8) as u8;
    };
    match clip {
        None => frame.chunks_exact_mut(4).for_each(&mut apply),
        Some(rects) => {
            let h = frame.len() / 4 / fw as usize;
            for &(rx, ry, rw, rh) in rects {
                for y in ry.max(0)..(ry + rh).min(h as i32) {
                    for x in rx.max(0)..(rx + rw).min(fw as i32) {
                        let i = (y as usize * fw as usize + x as usize) * 4;
                        apply(&mut frame[i..i + 4]);
                    }
                }
            }
        }
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrGammaControlManagerV1,
        request: gcm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let gcm::Request::GetGammaControl { id, output } = request {
            // Resolve which connector this wl_output names — the
            // ramp applies there, nowhere else.
            let name = state
                .outputs
                .iter()
                .find(|(o, _)| o.id() == output.id())
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            let ctl = init.init::<ZwlrGammaControlV1, GammaCtl>(id, name.clone());
            ctl.gamma_size(GAMMA_SIZE as u32);
            info!(output = %name, "gamma control granted");
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, GammaCtl> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        ctl: &ZwlrGammaControlV1,
        request: gc::Request,
        output: &GammaCtl,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            gc::Request::SetGamma { fd } => {
                // The fd carries 3×GAMMA_SIZE u16 in r,g,b order —
                // a short or unreadable table fails the request.
                // pread because the client's write left the file
                // offset at the end; the offset is shared state.
                let mut buf = vec![0u8; 3 * GAMMA_SIZE * 2];
                let n = rustix::io::pread(&fd, &mut buf, 0).unwrap_or(0);
                if n != buf.len() {
                    ctl.failed();
                    return;
                }
                let ramp: Vec<u16> = buf
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                // Ramp + owning control — the map entry dies
                // with the object, not just on explicit destroy.
                state
                    .gamma
                    .insert(output.clone(), (ctl.clone(), Arc::new(ramp)));
                state.damage_everything();
                info!(output, "gamma ramp applied");
            }
            gc::Request::Destroy => {
                // Protocol contract: destroying the control
                // restores the output's previous gamma.
                state.gamma.remove(output);
                state.damage_everything();
            }
            _ => {}
        }
    }
}
