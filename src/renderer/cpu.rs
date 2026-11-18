// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Software fallback — the existing CPU composite into a dumb
//! buffer. Always available; selected only when no GPU path exists.

use super::{Backend, OutputSpec};

pub struct CpuBackend {
    /// Packed RGBA scratch the scene is composited into before the
    /// copy into the (possibly padded) dumb buffer.
    pub pixels: Vec<u8>,
}

impl CpuBackend {
    pub fn new(_spec: OutputSpec) -> Self {
        Self { pixels: Vec::new() }
    }
}

impl Backend for CpuBackend {
    fn render(
        &mut self,
        comp: &mut crate::compositor::CompState,
        spec: OutputSpec,
        _cover_clip: Option<&[(i32, i32, i32, i32)]>,
        _cover_seq: u64,
        _blocked: u8,
        tick: &mut dyn FnMut(&mut crate::compositor::CompState),
    ) -> anyhow::Result<&[u8]> {
        let need = (spec.w * spec.h * 4) as usize;
        if self.pixels.len() != need {
            self.pixels = vec![0u8; need];
        }
        // Viewport composite — the output's global origin clips the
        // scene the same way on every backend. Draining the frame
        // damage here clips surface blits to what actually changed;
        // the same rects feed dirtyfb further up the present path.
        let damage = comp.present_damage();
        // Retained rendering: only damaged rects get a fresh
        // backdrop — pixels outside keep last frame's content.
        let local: Vec<(i32, i32, i32, i32)> = match &damage {
            None => vec![(0, 0, spec.w as i32, spec.h as i32)],
            Some(rs) => rs
                .iter()
                .map(|&(x, y, w, h)| (x - spec.ox, y - spec.oy, w, h))
                .collect(),
        };
        for &(rx, ry, rw, rh) in &local {
            for y in ry.max(0)..(ry + rh).min(spec.h as i32) {
                for x in rx.max(0)..(rx + rw).min(spec.w as i32) {
                    let i = (y as usize * spec.w as usize + x as usize) * 4;
                    self.pixels[i..i + 4].copy_from_slice(&[0x20, 0x10, 0x40, 0xff]);
                }
            }
        }
        comp.composite(
            &mut self.pixels,
            spec.w,
            spec.h,
            spec.ox,
            spec.oy,
            // A hardware cursor plane owns the pointer on this
            // output — compositing it would double-draw.
            !comp.hw_cursor.contains(&spec.name),
            damage.as_deref(),
        );
        // Cooperative yield — the composite is monolithic, so
        // input drains between it and the gamma pass rather
        // than inside it.
        tick(comp);
        // The output's gamma ramp applies at present like a
        // CRTC LUT — composites and captures stay pre-gamma. Only
        // damaged rects re-ramp: undamaged pixels already carry
        // last frame's LUT pass.
        if let Some(ramp) = comp.gamma_lut(&spec.name) {
            // Same local rects as the backdrop — `damage` being
            // Some means clip, None means whole frame.
            crate::compositor::gamma::apply_gamma(
                &mut self.pixels,
                spec.w,
                &ramp,
                damage.as_ref().map(|_| local.as_slice()),
            );
        }
        Ok(&self.pixels)
    }
}
