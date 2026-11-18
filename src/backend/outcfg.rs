// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Live output reconfiguration — the event loop drains the
//! compositor's `pending_outops` here and performs the real
//! hardware changes: CRTC re-modeset, viewport origin move.
//! The protocol layer already validated the request against the
//! connector's advertised mode list, so a `false` return from
//! `set_mode` means the DRM side itself refused — the caller
//! reconciles the layout back to hardware truth.

use drm::buffer::Buffer as _;
use drm::control::Device as ControlDevice;
use std::os::fd::{AsFd, AsRawFd};

use super::render::Output;
use crate::compositor::output_mgmt::OutOp;

impl Output {
    /// Switch the CRTC to another connector-advertised mode.
    /// Every buffer sized to the old mode comes down — dumb BO,
    /// its fb and map, the direct-scanout parities — the render
    /// backend is rebuilt for the new extent, and `crtc_set`
    /// flips back so the next frame modesets through the normal
    /// path. Returns false when no advertised mode matches.
    pub fn set_mode(&mut self, w: u32, h: u32, hz: u32) -> bool {
        // Pick the advertised mode matching the request: exact
        // (w,h), preferred vrefresh, else the nearest rate.
        let mode = self
            .conn
            .modes()
            .iter()
            .filter(|m| m.size().0 as u32 == w && m.size().1 as u32 == h)
            .min_by_key(|m| m.vrefresh().abs_diff(hz))
            .cloned();
        let Some(mode) = mode else {
            tracing::warn!(output = %self.name, w, h, hz, "requested mode not advertised");
            return false;
        };
        let old_h = self.mode.size().1 as usize;
        // Maps die before their BOs — munmap needs the pitch the
        // buffer was mapped with.
        if !self.map_ptr.is_null() {
            unsafe {
                libc::munmap(self.map_ptr as *mut libc::c_void, self.map_pitch * old_h);
            }
            self.map_ptr = std::ptr::null_mut();
            self.map_pitch = 0;
        }
        let scan_pitches: Vec<usize> = self.scan_dbs.iter().map(|d| d.pitch() as usize).collect();
        for (i, p) in self.scan_maps.drain(..).enumerate() {
            if !p.is_null() {
                unsafe {
                    libc::munmap(p as *mut libc::c_void, scan_pitches[i] * old_h);
                }
            }
        }
        for fb in self.scan_fbs.drain(..) {
            let _ = self.card.destroy_framebuffer(fb);
        }
        for db in self.scan_dbs.drain(..) {
            let _ = self.card.destroy_dumb_buffer(db);
        }
        if let Some(fb) = self.fb.take() {
            let _ = self.card.destroy_framebuffer(fb);
        }
        if let Some(db) = self.dumb.take() {
            let _ = self.card.destroy_dumb_buffer(db);
        }
        // Imported client fbs are mode-sized too — a remodeset
        // invalidates them like the scan parities.
        self.clear_direct_fbs();
        self.direct_dead = false;
        self.mode = mode;
        // Next frame re-runs the full modeset path — direct
        // scanout re-probes (BOs are mode-sized), damage starts
        // clean, no staged frame may present against old state.
        self.crtc_set = false;
        self.last_fb = None;
        self.flipped_parity = None;
        self.scan_tried = false;
        self.staged = None;
        self.pending_damage.clear();
        self.render_errors = 0;
        self.backend = crate::renderer::create(
            self.card.as_fd().as_raw_fd(),
            crate::renderer::OutputSpec {
                w,
                h,
                ox: self.origin.0,
                oy: self.origin.1,
                name: self.name.clone(),
            },
        );
        tracing::info!(output = %self.name, w, h, hz = mode.vrefresh(), "output re-modeset queued");
        true
    }

    /// Move this output's viewport origin in global space — the
    /// render spec already reads `self.origin` per frame, so the
    /// move is a field write plus a full repaint.
    pub fn set_pos(&mut self, x: i32, y: i32) {
        self.origin = (x, y);
        self.pending_damage.clear();
    }
}

/// Drain the compositor's queued output ops against the live
/// DRM outputs — runs once per frame tick. A failed re-modeset
/// reconciles the layout entry back to the mode the hardware
/// actually drives, then re-syncs heads so clients see truth.
pub fn apply_outops(state: &mut super::eventloop::CompState) {
    if state.comp.pending_outops.is_empty() {
        return;
    }
    let ops = std::mem::take(&mut state.comp.pending_outops);
    let mut resync = false;
    for op in ops {
        match op {
            OutOp::Mode { name, w, h, hz } => {
                let Some(out) = state.outputs.iter_mut().find(|o| o.name == name) else {
                    continue;
                };
                if !out.set_mode(w, h, hz) {
                    let (mw, mh) = out.mode.size();
                    if let Some(lo) = state.comp.layout.outputs.get_mut(&name) {
                        lo.mode = (mw as u32, mh as u32, out.mode.vrefresh());
                    }
                    resync = true;
                    tracing::warn!(output = %name, "re-modeset failed — layout reverted");
                }
            }
            OutOp::Position { name, x, y } => {
                if let Some(out) = state.outputs.iter_mut().find(|o| o.name == name) {
                    out.set_pos(x, y);
                }
            }
            // Disable: blank immediately rather than waiting for
            // the next frame's enabled check — re-enable simply
            // resumes through frame().
            OutOp::Enabled { name, on } => {
                if !on {
                    if let Some(out) = state.outputs.iter_mut().find(|o| o.name == name) {
                        if out.crtc_set {
                            let _ = out.card.set_crtc(out.crtc, None, (0, 0), &[], None);
                            out.crtc_set = false;
                            out.last_fb = None;
                        }
                    }
                }
            }
        }
    }
    if resync {
        state.comp.om_sync();
        state.comp.om_sync_wl_outputs();
    }
    state.comp.dirty = true;
}
