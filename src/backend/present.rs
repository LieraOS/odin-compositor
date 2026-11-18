// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `Output::present` + `Output::settle` — copy the rendered frame
//! into the scanout dumb buffer (damage-clipped) and flip, plus
//! the idle-tick pass that drains a pipelined backend's in-flight
//! frame. Split from render.rs to keep both files inside the size
//! budget.

use drm::buffer::Buffer as _;
use drm::control::Device as ControlDevice;
use std::os::fd::AsFd;

use super::render::{set_master, union_damage, Output};

impl Output {
    /// Copy pixels into the scanout buffer (damage-clipped) and
    /// present — shared by `frame()` and `settle()`. `px_ptr` is
    /// raw so the backend borrow ends before `&mut self` runs.
    /// `input` feeds the cooperative yield inside long copies —
    /// a ~9 MB write into write-combined scanout can take tens
    /// of ms, and input+cursor must not freeze for it.
    /// `scan_fb` is the direct-scanout framebuffer for this
    /// frame's parity — `Some` turns the present into a bare
    /// page-flip and skips the readback→dumb copy entirely.
    pub(super) fn present(
        &mut self,
        px_ptr: *const u8,
        clip: Option<&[(i32, i32, i32, i32)]>,
        comp: &mut crate::compositor::CompState,
        input: &mut input::Libinput,
        scan_fb: Option<drm::control::framebuffer::Handle>,
    ) {
        let (w, h) = self.mode.size();
        let (w, h) = (w as u32, h as u32);
        let px = unsafe { std::slice::from_raw_parts(px_ptr, (w * h * 4) as usize) };
        // Frame dump — BGRA → PPM (RGB) into the state dir, one
        // file per output. Every output dumps once per dump_seq
        // generation, so all monitors are captured, not just the
        // first to present. The readback is host-mapped, so this
        // works on the direct-scanout path too.
        if comp.dump_seq != self.dump_done {
            self.dump_done = comp.dump_seq;
            let dir = std::env::var("XDG_STATE_HOME")
                .map(|d| format!("{d}/odin"))
                .unwrap_or_else(|_| {
                    format!(
                        "{}/.local/state/odin",
                        std::env::var("HOME").unwrap_or_default()
                    )
                });
            let _ = std::fs::create_dir_all(&dir);
            let path = format!("{dir}/dump-{}-{}.ppm", self.origin.0, self.origin.1);
            // px can point into a write-combined dumb-BO mapping —
            // per-pixel reads there stall for seconds (the ~16 s
            // freeze this code used to cause). One bulk copy into
            // cached RAM is orders of magnitude faster; the PPM
            // conversion + disk write then run on a worker thread
            // so the event loop keeps pumping.
            let mut raw = vec![0u8; px.len()];
            raw.copy_from_slice(px);
            std::thread::spawn(move || {
                let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
                out.reserve_exact(raw.len() / 4 * 3);
                for p in raw.chunks_exact(4) {
                    out.extend_from_slice(&[p[2], p[1], p[0]]);
                }
                match std::fs::write(&path, &out) {
                    Ok(()) => tracing::info!(path, "frame dumped"),
                    Err(err) => tracing::warn!(?err, path, "frame dump failed"),
                }
            });
        }
        if let Some(fb) = scan_fb {
            // Zero-copy present — the retired readback IS the
            // framebuffer. Flip to it; no CPU touch at all.
            if self.flip_or_modeset(fb, comp) {
                comp.frame_done(&self.name);
            }
            return;
        }
        if self.dumb.is_none() {
            tracing::info!("drm: create_dumb_buffer...");
            let mut db =
                match self
                    .card
                    .create_dumb_buffer((w, h), drm::buffer::DrmFourcc::Xrgb8888, 32)
                {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::warn!(?err, "dumb buffer create failed");
                        return;
                    }
                };
            tracing::info!("drm: dumb buffer ok");
            // depth 24 / bpp 32 matches XRGB8888.
            tracing::info!("drm: add_framebuffer...");
            let fb = match self.card.add_framebuffer(&db, 24, 32) {
                Ok(f) => f,
                Err(err) => {
                    tracing::warn!(?err, "add_framebuffer failed");
                    return;
                }
            };
            tracing::info!("drm: framebuffer ok");
            // Map the dumb buffer ONCE — the mapping lives for the
            // rest of the session. Per-frame map_dumb_buffer is an
            // ioctl + mmap + munmap churn on ~9 MB.
            let pitch = db.pitch() as usize;
            match self.card.map_dumb_buffer(&mut db) {
                Ok(mut map) => {
                    self.map_ptr = map.as_mut().as_mut_ptr();
                    self.map_pitch = pitch;
                    // Deliberately leaked: munmap only runs on
                    // Drop, and this mapping must outlive `db`'s
                    // borrow — the process exit cleans it up.
                    std::mem::forget(map);
                }
                Err(err) => {
                    tracing::warn!(?err, "dumb buffer map failed");
                    return;
                }
            }
            self.dumb = Some(db);
            self.fb = Some(fb);
        }
        // Copy the clip region into the scanout buffer — `None`
        // means the whole frame. Rects arrive in global space.
        let t_copy = std::time::Instant::now();
        {
            let dst = unsafe {
                std::slice::from_raw_parts_mut(self.map_ptr, self.map_pitch * h as usize)
            };
            let mut t_yield = std::time::Instant::now();
            match &clip {
                Some(rects) => {
                    for &(x, y, rw, rh) in rects.iter() {
                        let (x, y) = (x - self.origin.0, y - self.origin.1);
                        let (x0, y0) = (x.max(0), y.max(0));
                        let (x1, y1) = ((x + rw).min(w as i32), (y + rh).min(h as i32));
                        if x1 <= x0 || y1 <= y0 {
                            continue;
                        }
                        let row_bytes = (x1 - x0) as usize * 4;
                        for yy in y0..y1 {
                            let s = (yy as usize * w as usize + x0 as usize) * 4;
                            let d = yy as usize * self.map_pitch + x0 as usize * 4;
                            dst[d..d + row_bytes].copy_from_slice(&px[s..s + row_bytes]);
                            // Yield every ~8 ms — a large clip
                            // into WC scanout can take tens of ms.
                            if t_yield.elapsed().as_millis() >= 8 {
                                t_yield = std::time::Instant::now();
                                let _ = super::input::drain(input, comp);
                                self.sync_cursor(comp);
                            }
                        }
                    }
                }
                None => {
                    let row = (w * 4) as usize;
                    for y in 0..h as usize {
                        let d = y * self.map_pitch;
                        dst[d..d + row].copy_from_slice(&px[y * row..(y + 1) * row]);
                        if t_yield.elapsed().as_millis() >= 8 {
                            t_yield = std::time::Instant::now();
                            let _ = super::input::drain(input, comp);
                            self.sync_cursor(comp);
                        }
                    }
                }
            }
        }
        comp.stat_copy_ms = comp
            .stat_copy_ms
            .max(t_copy.elapsed().as_secs_f64() * 1000.0);
        let fb = self.fb.unwrap();
        // Framebuffer-space dirty rects for needs_dirtyfb drivers
        // (virtio) — the same clip used for the memcpy, clipped to
        // the fb. `None` (structural change) flushes everything;
        // an all-clipped-away set flushes nothing at all.
        let mut fb_rects: Vec<drm::control::ClipRect> = Vec::new();
        match &clip {
            Some(rects) => {
                for &(x, y, rw, rh) in rects.iter() {
                    let x = x - self.origin.0;
                    let y = y - self.origin.1;
                    let (x0, y0) = (x.max(0), y.max(0));
                    let (x1, y1) = ((x + rw).min(w as i32), (y + rh).min(h as i32));
                    if x1 > x0 && y1 > y0 {
                        fb_rects.push(drm::control::ClipRect::new(
                            x0 as u16, y0 as u16, x1 as u16, y1 as u16,
                        ));
                    }
                }
            }
            None => fb_rects.push(drm::control::ClipRect::new(0, 0, w as u16, h as u16)),
        }
        if !self.flip_or_modeset(fb, comp) {
            return; // modeset denied — nothing presented
        }
        // Dirtyfb push after the scanout is pointing at this fb —
        // tells host-backed drivers (virtio) which rects changed.
        let t_dirty = std::time::Instant::now();
        if self.crtc_set && self.needs_dirtyfb && !fb_rects.is_empty() {
            let _ = self.card.dirty_framebuffer(fb, &fb_rects);
        }
        comp.stat_dirty_ms = comp
            .stat_dirty_ms
            .max(t_dirty.elapsed().as_secs_f64() * 1000.0);
        // Clients' frame callbacks fire on the present — the same
        // pacing semantics as the nested backend.
        comp.frame_done(&self.name);
    }

    /// Modeset-or-flip shared by the dumb-copy and direct-scanout
    /// presents: `set_crtc` while the output isn't live yet
    /// (with the master/EACCES retry ladder), `page_flip` when
    /// the fb actually changed, nothing otherwise. Returns false
    /// when nothing reached the CRTC — the caller then skips
    /// frame callbacks so clients don't pace on a failed present.
    pub(crate) fn flip_or_modeset(
        &mut self,
        fb: drm::control::framebuffer::Handle,
        comp: &mut crate::compositor::CompState,
    ) -> bool {
        let t_flip = std::time::Instant::now();
        if !self.crtc_set {
            tracing::info!("drm: set_crtc...");
            if let Err(err) = self.card.set_crtc(
                self.crtc,
                Some(fb),
                (0, 0),
                &[self.conn.handle()],
                Some(self.mode),
            ) {
                // EACCES = not DRM master yet. The old server drops
                // master asynchronously on the VT switch — retry
                // SET_MASTER here; after ~5 s of denials we exit so
                // the display manager restores its greeter instead
                // of leaving the user on a dead black screen.
                if err.raw_os_error() == Some(libc::EACCES) {
                    self.master_denials += 1;
                    if self.master_denials % 60 == 1 {
                        let _ = set_master(self.card.as_fd());
                        tracing::warn!(
                            denials = self.master_denials,
                            "set_crtc EACCES — retried set_master"
                        );
                    }
                    if self.master_denials > 300 {
                        self.fatal = true;
                        tracing::error!("modesetting denied for 5 s — exiting to display manager");
                    }
                } else {
                    // EINVAL on a dmabuf-imported fb means the
                    // driver can't scan out that memory domain —
                    // direct scanout is dead for this output, so
                    // drop the fbs and let the dumb-copy path
                    // present the very next frame instead of
                    // retrying the same broken fb forever.
                    if !self.scan_fbs.is_empty() {
                        tracing::warn!(
                            ?err,
                            "set_crtc rejected scanout fb — disabling direct scanout"
                        );
                        self.scan_fbs.clear();
                    } else {
                        tracing::warn!(?err, "set_crtc failed");
                    }
                }
                return false;
            }
            self.crtc_set = true;
            self.last_fb = Some(fb);
            // A modeset lands the fb on the CRTC immediately —
            // same protection as a pending flip.
            self.flipped_parity = self.scan_fbs.iter().position(|&f| f == fb);
            self.master_denials = 0;
            tracing::info!("udev output: mode set, first frame presented");
        } else if self.last_fb != Some(fb) {
            // The scanout fb actually changed — flip to it. With
            // the single-front-buffer design this branch never
            // runs after the modeset: every later frame lands in
            // the same BO and the dirtyfb above is the only push
            // virtio needs. A flip on the unchanged fb is a
            // wasted host round-trip (~27 ms on virtio-gpu).
            // wp_tearing_control_v1 — a committed async hint
            // requests ASYNC_PAGE_FLIP (no vblank wait). Drivers
            // without async-flip support reject it; fall back to
            // the normal flip rather than dropping the frame.
            let flags = if comp.tearing_active() {
                drm::control::PageFlipFlags::ASYNC
            } else {
                drm::control::PageFlipFlags::empty()
            };
            let flipped = self.card.page_flip(self.crtc, fb, flags, None).is_ok()
                || (flags == drm::control::PageFlipFlags::ASYNC
                    && self
                        .card
                        .page_flip(self.crtc, fb, drm::control::PageFlipFlags::empty(), None)
                        .is_ok());
            if !flipped {
                // EINVAL on a client-imported fb means the driver
                // cannot scan out that layout — the caller marks
                // client direct-scanout dead and composites
                // instead. Never claim a fb the CRTC rejected.
                comp.stat_flip_ms = comp
                    .stat_flip_ms
                    .max(t_flip.elapsed().as_secs_f64() * 1000.0);
                return false;
            }
            self.last_fb = Some(fb);
            // The flip is in flight — its parity is untouchable
            // until a newer flip lands. Maps to None on the
            // legacy dumb fb, which needs no protection.
            self.flipped_parity = self.scan_fbs.iter().position(|&f| f == fb);
        }
        comp.stat_flip_ms = comp
            .stat_flip_ms
            .max(t_flip.elapsed().as_secs_f64() * 1000.0);
        true
    }

    /// Present the in-flight frame once its fence retires —
    /// pipelined backends strand their newest pixels GPU-side
    /// until more damage arrives; this drains them on idle
    /// ticks instead. Runs every event-loop pass: a fence
    /// status check, a clipped copy, a flip. `input` feeds the
    /// copy's cooperative yield like `frame_present`.
    pub fn settle(&mut self, comp: &mut crate::compositor::CompState, input: &mut input::Libinput) {
        // Client direct-scanout owns the CRTC right now — a
        // retired composite frame would overwrite the scanned
        // client buffer with stale pixels. Drain it so the
        // backend's pipeline stays consistent, but never present.
        if self.direct_buf.is_some() {
            self.backend.take_retired();
            return;
        }
        let Some(px_ptr) = self.backend.take_retired() else {
            return;
        };
        // Direct scanout: the retired pointer is a stale readback
        // — the real pixels live in the parity's dumb BO map.
        let px_ptr = self
            .backend
            .last_retired()
            .and_then(|i| self.scan_maps.get(i).copied())
            .map(|p| p as *const u8)
            .unwrap_or(px_ptr);
        // Only damage inside the just-retired readback may come
        // out of the queue — sets a later submission still has to
        // cover stay put, and non-touching sets drain for free.
        let presentable = self.backend.presentable_seq();
        let (ox, oy) = (self.origin.0, self.origin.1);
        let (w, h) = self.mode.size();
        let mut sets = Vec::new();
        while let Some((s, set)) = self.pending_damage.front() {
            let touched = match set {
                None => true,
                Some(rs) => rs.iter().any(|&(x, y, rw, rh)| {
                    x + rw > ox && y + rh > oy && x < ox + w as i32 && y < oy + h as i32
                }),
            };
            if *s <= presentable || !touched {
                sets.push(self.pending_damage.pop_front().unwrap().1);
            } else {
                break;
            }
        }
        let clip = if self.crtc_set {
            union_damage(sets)
        } else {
            None
        };
        if let Some(rs) = &clip {
            let touched = rs.iter().any(|&(x, y, rw, rh)| {
                x + rw > ox && y + rh > oy && x < ox + w as i32 && y < oy + h as i32
            });
            if !touched {
                comp.frame_done(&self.name);
                return;
            }
        }
        let fb = self
            .backend
            .last_retired()
            .and_then(|i| self.scan_fbs.get(i).copied());
        self.present(px_ptr, clip.as_deref(), comp, input, fb);
    }
}
