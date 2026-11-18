// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame damage accounting — client-reported surface damage and
//! structural scene changes land here and are consumed once per
//! present sweep by `present_damage`.

use super::CompState;

impl CompState {
    /// Drain the accumulated damage for this present —
    /// `None` means the whole frame composites (structural
    /// change); `Some(rects)` is the clip list, empty means
    /// nothing visually changed but a frame was requested.
    /// Memoized per frame sweep: every output composites from
    /// the SAME damage set — the backend resets `damage_taken`
    /// before its outputs loop.
    pub fn present_damage(&mut self) -> Option<Vec<(i32, i32, i32, i32)>> {
        if self.damage_taken {
            return self.frame_damage.clone();
        }
        self.damage_taken = true;
        self.frame_damage = if self.damage_full {
            self.damage_full = false;
            self.damage.clear();
            self.stat_full += 1;
            None
        } else {
            Some(crate::compositor::render::merge_damage(std::mem::take(
                &mut self.damage,
            )))
        };
        self.stat_frames += 1;
        // A sustained full-damage stream is the "mouse feels
        // slow" signature — surface it in the debug log with the
        // frame count instead of leaving it invisible.
        if self.stat_last.elapsed().as_secs() >= 5 && self.stat_frames > 0 {
            tracing::info!(
                frames = self.stat_frames,
                full = self.stat_full,
                frame_ms_max = self.stat_frame_ms.round() as u64,
                render_ms = self.stat_render_ms.round() as u64,
                present_ms = self.stat_present_ms.round() as u64,
                copy_ms = self.stat_copy_ms.round() as u64,
                dirty_ms = self.stat_dirty_ms.round() as u64,
                flip_ms = self.stat_flip_ms.round() as u64,
                input_lag_ms = self.stat_input_ms.round() as u64,
                vk_wait_ms = self.stat_wait_ms.round() as u64,
                vk_upload_ms = self.stat_upload_ms.round() as u64,
                vk_draw_ms = self.stat_draw_ms.round() as u64,
                vk_submit_ms = self.stat_submit_ms.round() as u64,
                pump_ms = self.stat_pump_ms.round() as u64,
                xwm_ms = self.stat_xwm_ms.round() as u64,
                drain_ms = self.stat_drain_ms.round() as u64,
                up_kb = self.stat_up_kb,
                up_texs = self.stat_up_texs,
                up_big_kb = self.stat_up_big_kb,
                up_big = format_args!("{}x{}", self.stat_up_big_w, self.stat_up_big_h),
                "present stats"
            );
            self.stat_frames = 0;
            self.stat_full = 0;
            self.stat_frame_ms = 0.0;
            self.stat_render_ms = 0.0;
            self.stat_present_ms = 0.0;
            self.stat_copy_ms = 0.0;
            self.stat_dirty_ms = 0.0;
            self.stat_flip_ms = 0.0;
            self.stat_input_ms = 0.0;
            self.stat_wait_ms = 0.0;
            self.stat_upload_ms = 0.0;
            self.stat_draw_ms = 0.0;
            self.stat_submit_ms = 0.0;
            self.stat_pump_ms = 0.0;
            self.stat_xwm_ms = 0.0;
            self.stat_drain_ms = 0.0;
            self.stat_up_kb = 0;
            self.stat_up_texs = 0;
            self.stat_up_big_kb = 0;
            self.stat_up_big_w = 0;
            self.stat_up_big_h = 0;
            self.stat_last = std::time::Instant::now();
        }
        self.frame_damage.clone()
    }

    /// Mark the next frame fully damaged — structural changes
    /// (map, unmap, retile, move, ws switch, minimize) invalidate
    /// regions client damage never covers.
    pub fn damage_everything(&mut self) {
        self.damage_full = true;
        self.dirty = true;
    }
}
