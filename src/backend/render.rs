// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! First real pixels: GBM buffer allocation + EGL/GLES rendering
//! pushed to a CRTC over the legacy modeset path.
//!
//! The pipeline is the standard KMS one every compositor shares:
//! GBM surface (scanout-capable buffers) → EGL surface on top →
//! GLES renders → swap → front BO → drmAddFB → set_crtc/page_flip.
//! Atomic commits replace the legacy call once damage tracking
//! lands — the buffer plumbing is identical either way.

use drm::buffer::Buffer as _;
use drm::control::Device as ControlDevice;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

/// Adapter making a session device fd usable as drm::Device.
pub(crate) struct Card(pub(crate) std::os::fd::OwnedFd);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// A live output on real hardware: card + crtc + mode + GBM
/// scanout buffer. `frame()` composites the scene into a CPU
/// frame, writes it into the BO, and page-flips — the udev
/// equivalent of the nested present path.
pub struct Output {
    pub(crate) card: Card,
    pub(crate) conn: drm::control::connector::Info,
    /// Kernel connector name — per-output compositor state
    /// (gamma ramp, geometry) keys off it.
    pub(crate) name: String,
    pub(crate) crtc: drm::control::crtc::Handle,
    pub(crate) mode: drm::control::Mode,
    /// This output's origin in global layout space — surfaces and
    /// the pointer are positioned globally, the viewport clips.
    pub(crate) origin: (i32, i32),
    /// CPU-mappable dumb buffer we composite into — always linear
    /// and scanout-capable on every driver, unlike driver-chosen
    /// GBM layouts that legacy add_framebuffer rejects (EINVAL seen
    /// on NVIDIA and virtio-gpu alike).
    pub(crate) dumb: Option<drm::control::dumbbuffer::DumbBuffer>,
    pub(crate) fb: Option<drm::control::framebuffer::Handle>,
    /// fb currently on the CRTC — a page_flip only runs when it
    /// differs, since the single-front-buffer design presents
    /// into one BO and a flip on the unchanged fb is a pure
    /// host round-trip (~27 ms on virtio-gpu).
    pub(crate) last_fb: Option<drm::control::framebuffer::Handle>,
    pub(crate) crtc_set: bool,
    /// The render backend — vulkan/gles/cpu chosen at creation;
    /// produces the packed BGRA frame this output presents.
    pub(crate) backend: Box<dyn crate::renderer::Backend>,
    /// virtio-gpu only pushes guest memory to the host scanout on
    /// an explicit DRM_MODE_DIRTYFB — real drivers (nvidia, i915,
    /// amdgpu) map dumb buffers coherently and their dirtyfb
    /// implementations are a known wedge risk, so it stays off.
    pub(crate) needs_dirtyfb: bool,
    /// Consecutive set_crtc EACCES failures — used to retry
    /// DRM_IOCTL_SET_MASTER while the old display server releases
    /// the card, then to give up rather than sit on a black screen.
    pub(crate) master_denials: u32,
    /// Set when modesetting is permanently denied — the event loop
    /// exits cleanly so the display manager takes back over
    /// instead of the user facing a dead black screen.
    pub fatal: bool,
    /// Consecutive backend render failures — at the threshold the
    /// output demotes to the CPU renderer for the rest of the run.
    pub(crate) render_errors: u32,
    /// Per-frame damage sets not yet copied into the dumb buffer,
    /// each tagged with a monotonically increasing sequence
    /// number. A pipelined backend only reads back the union of
    /// whatever was queued at submit time — a set may therefore
    /// be presented only from a readback whose submission covered
    /// it, which is exactly what `seq ≤ backend.presentable_seq()`
    /// tests. Sets that never touch this output need no coverage:
    /// they contribute nothing when they drain, so they leave the
    /// queue unconditionally instead of stalling the pipeline.
    pub(crate) pending_damage: std::collections::VecDeque<(u64, DamageSet)>,
    /// Sequence counter backing `pending_damage` tags.
    pub(crate) damage_seq: u64,
    /// Persistent map of the dumb buffer — `map_dumb_buffer` per
    /// frame is an ioctl + mmap + munmap round trip on ~9 MB of
    /// write-combined memory; mapped once here instead.
    pub(crate) map_ptr: *mut u8,
    /// Dumb buffer pitch — rows in the map may be padded.
    pub(crate) map_pitch: usize,
    /// Rendered frame staged between `frame()` and
    /// `frame_present()` — lets the event loop drain input in
    /// the gap instead of making pointer events wait out the
    /// whole render+copy+present pass.
    pub(crate) staged: Option<StagedPresent>,
    /// Hardware cursor plane for this CRTC — pointer motion
    /// becomes a bare ioctl instead of a composited repaint
    /// once the driver accepts it.
    pub hwcursor: super::hwcursor::HwCursor,
    /// DRM framebuffers wrapping the backend's exported readback
    /// parities — a direct-scanout present flips to one of these
    /// and skips the readback→dumb CPU copy entirely. Empty =
    /// legacy copy path (export or FB creation failed).
    pub(crate) scan_fbs: Vec<drm::control::framebuffer::Handle>,
    /// The dmabuf→FB import probe ran — avoids retrying ioctls
    /// every frame once direct scanout is known impossible.
    pub(crate) scan_tried: bool,
    /// The three scannable dumb BOs behind `scan_fbs` — the DRM
    /// driver allocated them, so the CRTC accepts them by
    /// construction; the backend's imported images render
    /// straight into this memory.
    pub(crate) scan_dbs: Vec<drm::control::dumbbuffer::DumbBuffer>,
    /// CPU mappings of `scan_dbs`, index-matched — the direct
    /// path's pixel pointer for dump/screencopy reads.
    pub(crate) scan_maps: Vec<*mut u8>,
    /// The parity a page_flip most recently targeted — pending
    /// or freshly scanned, either way untouchable until a newer
    /// flip replaces it. Closes the race where get_crtc still
    /// reports the previous buffer while the flip is in flight.
    pub(crate) flipped_parity: Option<usize>,
    /// Client dmabufs imported as scanout framebuffers — the
    /// direct-scanout path flips a lone fullscreen surface's own
    /// buffer straight to the CRTC, keyed by wl_buffer id.
    pub(crate) direct_fbs: Vec<super::direct::ClientFb>,
    /// wl_buffer id currently scanned out directly — `Some`
    /// while this output presents through client buffers
    /// instead of the composited parity/dumb path.
    pub(crate) direct_buf: Option<wayland_server::backend::ObjectId>,
    /// Client-buffer scanout proven impossible on this output —
    /// stops the import/flip probe from spamming ioctls every
    /// frame after the first rejection.
    pub(crate) direct_dead: bool,
    /// Last frame-dump generation this output wrote — compared
    /// against CompState::dump_seq so each dump-frame keypress
    /// yields exactly one file per output, whichever order the
    /// outputs present in.
    pub(crate) dump_done: u64,
}

/// One frame's damage contribution — `None` marks a structural
/// change that forces a full copy; `Some` is a rect list in
/// global scene space.
pub(crate) type DamageSet = Option<Vec<(i32, i32, i32, i32)>>;

/// A rendered frame waiting for its scanout copy — `frame()`
/// stages one so the event loop can interleave an input drain
/// between the render pass and the copy+present, the two
/// heaviest blocking phases of a frame. `fb` is the direct-
/// scanout framebuffer when the backend exported this parity —
/// present() then flips instead of copying.
pub(crate) struct StagedPresent {
    pub px_ptr: *const u8,
    pub clip: DamageSet,
    pub fb: Option<drm::control::framebuffer::Handle>,
}

/// Fold per-frame damage sets into one clip — `None` anywhere
/// means a structural change forces a full copy. Sets append
/// in frame order; merging compacts them for the memcpy.
pub(crate) fn union_damage(sets: Vec<DamageSet>) -> DamageSet {
    let mut rects = Vec::new();
    for s in sets {
        match s {
            None => return None,
            Some(rs) => rects.extend(rs),
        }
    }
    Some(crate::compositor::render::merge_damage(rects))
}

/// `DRM_IOCTL_SET_MASTER` — `_IO('d', 0x1e)`. Needed after the VT
/// switch: seatd may have handed us the fd while the greeter still
/// held master, and for the seatd backend the client owns the
/// retry. EINVAL means "already master" and counts as success.
pub(crate) fn set_master(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x641e;
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_SET_MASTER as libc::Ioctl) };
    let err = std::io::Error::last_os_error();
    if rc == 0 || err.raw_os_error() == Some(libc::EINVAL) {
        Ok(())
    } else {
        Err(err)
    }
}

impl Output {
    /// True once the CRTC has been modeset — until then the frame
    /// timer must keep calling `frame()` so the EACCES/master
    /// retry path stays alive even without scene damage.
    pub fn presented(&self) -> bool {
        self.crtc_set
    }

    /// Composite the scene into the scanout buffer and present.
    /// `input` feeds the cooperative tick the backend calls
    /// inside long render phases — input events drain and the
    /// cursor plane moves instead of freezing for the whole pass.
    pub fn frame(&mut self, comp: &mut crate::compositor::CompState, input: &mut input::Libinput) {
        // zwlr_output_power_manager_v1 + output-management
        // disable — a powered-down or config-disabled output
        // blanks its CRTC once (`set_crtc(None)` is the legacy-KMS
        // DPMS equivalent) and skips compositing entirely until
        // it comes back.
        let enabled = comp
            .layout
            .outputs
            .get(&self.name)
            .map(|o| o.enabled)
            .unwrap_or(true);
        let on = enabled && comp.powered(&self.name);
        if !on && self.crtc_set {
            let _ = self.card.set_crtc(self.crtc, None, (0, 0), &[], None);
            self.crtc_set = false;
            self.last_fb = None;
            tracing::info!(output = %self.name, enabled, "output off — CRTC blanked");
        }
        if !on {
            return; // off: nothing to present
        }
        // Client-buffer direct scanout — a lone fullscreen
        // dmabuf surface covering this output flips its own
        // buffer to the CRTC: no render pass, no readback, no
        // dumb-copy. Handles its own damage bookkeeping and
        // frame_done pacing; returns false whenever the scene
        // needs real compositing.
        if self.direct_frame(comp, input) {
            return;
        }
        let (w, h) = self.mode.size();
        let (w, h) = (w as u32, h as u32);
        // This frame's damage joins the queue BEFORE the render —
        // even a failed render keeps it for the next attempt.
        self.damage_seq += 1;
        let new_seq = self.damage_seq;
        self.pending_damage
            .push_back((new_seq, comp.present_damage()));
        if self.pending_damage.len() > 8 {
            // Cap: a client out-committing the present collapses
            // into one merged set — a heavier copy, never stale.
            // The merged set keeps the newest covered sequence.
            let all: Vec<_> = self.pending_damage.drain(..).collect();
            let max_seq = all.iter().map(|e| e.0).max().unwrap_or(new_seq);
            self.pending_damage.push_back((
                max_seq,
                union_damage(all.into_iter().map(|e| e.1).collect()),
            ));
        }
        // Which damage actually lives inside the pixels the backend
        // can hand out: only sets its last retired submission read
        // back. `latency` stays as a second, conservative bound.
        let presentable = self.backend.presentable_seq();
        let lat = self.backend.latency();
        let (ox, oy) = self.origin;
        let touches = |set: &DamageSet| -> bool {
            match set {
                None => true,
                Some(rs) => rs.iter().any(|&(x, y, rw, rh)| {
                    x + rw > ox && y + rh > oy && x < ox + w as i32 && y < oy + h as i32
                }),
            }
        };
        // Drain the leading sets that are either provably inside
        // the retired readback or irrelevant to this output —
        // non-touching sets carry no pixels for us either way and
        // must not block coverage bookkeeping forever.
        let usable = self.pending_damage.len().saturating_sub(lat);
        let mut sets: Vec<(u64, DamageSet)> = Vec::new();
        while sets.len() < usable {
            match self.pending_damage.front() {
                Some((s, set)) if *s <= presentable || !touches(set) => {
                    sets.push(self.pending_damage.pop_front().unwrap());
                }
                _ => break,
            }
        }
        let clip = if self.crtc_set {
            union_damage(sets.iter().map(|e| e.1.clone()).collect())
        } else {
            None // pre-modeset frames are always full
        };
        let touched = match &clip {
            None => true,
            Some(rs) => {
                !rs.is_empty()
                    && rs.iter().any(|&(x, y, rw, rh)| {
                        x + rw > ox && y + rh > oy && x < ox + w as i32 && y < oy + h as i32
                    })
            }
        };
        // Coverage starvation: a queued set that touches this
        // output but is not inside the retired readback can only
        // present once a submission covers it — render even when
        // nothing drains this pass, or the set would stall.
        let needs_cover = self
            .pending_damage
            .iter()
            .any(|(s, set)| *s > presentable && touches(set));
        if !touched && !needs_cover {
            // Drained sets contributed nothing to this output —
            // drop them rather than requeue, so non-touching
            // damage cannot churn the queue forever.
            comp.frame_done(&self.name);
            return;
        }
        // The submission's readback covers the union of everything
        // still queued — output-local pixels, `None` (a structural
        // set in the queue) drains the whole frame.
        let mut cover_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
        let mut cover_full = false;
        for (_, set) in &self.pending_damage {
            match set {
                None => {
                    cover_full = true;
                    break;
                }
                Some(rs) => {
                    cover_rects.extend(rs.iter().map(|&(x, y, rw, rh)| (x - ox, y - oy, rw, rh)))
                }
            }
        }
        let cover_clip: Option<Vec<(i32, i32, i32, i32)>> = if cover_full {
            None
        } else {
            Some(crate::compositor::render::merge_damage(cover_rects))
        };
        let cover_seq = self.pending_damage.back().map(|e| e.0).unwrap_or(0);
        // Render failure must not drop the consumed sets — they go
        // back to the queue so the next frame still repaints them.
        macro_rules! requeue {
            () => {
                for s in sets.into_iter().rev() {
                    self.pending_damage.push_front(s);
                }
            };
        }
        // Direct-scanout probe — once per output. Import
        // direction, deliberately: the DRM driver allocates the
        // scanout memory itself (dumb BOs are CRTC-compatible by
        // construction — exporting host memory the other way is
        // rejected by drivers whose display engine can't scan
        // generic sysmem). The backend imports each BO's prime fd
        // as a render target; on success, presents become bare
        // page-flips and the readback→dumb CPU copy disappears.
        if !self.scan_tried {
            self.scan_tried = true;
            let mut planes: Vec<crate::renderer::DmaBufPlane> = Vec::new();
            let mut ok = true;
            for _ in 0..3 {
                let mut db =
                    match self
                        .card
                        .create_dumb_buffer((w, h), drm::buffer::DrmFourcc::Xrgb8888, 32)
                    {
                        Ok(d) => d,
                        Err(err) => {
                            tracing::warn!(?err, "scanout dumb BO failed");
                            ok = false;
                            break;
                        }
                    };
                let ptr = match self.card.map_dumb_buffer(&mut db) {
                    Ok(mut m) => {
                        let p = m.as_mut().as_mut_ptr();
                        std::mem::forget(m);
                        p
                    }
                    Err(err) => {
                        tracing::warn!(?err, "scanout BO map failed");
                        ok = false;
                        break;
                    }
                };
                match self
                    .card
                    .buffer_to_prime_fd(db.handle(), libc::O_CLOEXEC as u32)
                {
                    Ok(fd) => {
                        planes.push(crate::renderer::DmaBufPlane {
                            fd,
                            w,
                            h,
                            pitch: db.pitch(),
                        });
                        self.scan_dbs.push(db);
                        self.scan_maps.push(ptr);
                    }
                    Err(err) => {
                        tracing::warn!(?err, "prime fd export failed");
                        ok = false;
                        break;
                    }
                }
            }
            if ok && self.backend.attach_scanout(&planes) {
                for db in &self.scan_dbs {
                    match self.card.add_framebuffer(db, 24, 32) {
                        Ok(fb) => self.scan_fbs.push(fb),
                        Err(err) => {
                            tracing::warn!(?err, "scanout fb failed");
                            self.scan_fbs.clear();
                            break;
                        }
                    }
                }
            }
            if self.scan_fbs.len() == 3 {
                tracing::info!(output = %self.name, "direct scanout: rendering into dumb BOs");
            } else {
                // Half-built state would mislead the flip path —
                // everything imported so far unwinds here.
                self.scan_fbs.clear();
                self.scan_dbs.clear();
                self.scan_maps.clear();
            }
        }
        // Parities the GPU must not touch this frame, as a
        // bitmask: the fb the CRTC scans right now AND the fb a
        // pending page-flip will switch to. get_crtc alone is
        // racy — between flip submission and vblank it still
        // reports the old buffer, so the just-flipped parity
        // rides along explicitly. Rendering into either would
        // write mid-scan; on drivers that serialize scanout BO
        // writes it wedges the submission outright.
        let mut blocked = 0u8;
        if self.scan_fbs.len() == 3 {
            if let Some(i) = self
                .card
                .get_crtc(self.crtc)
                .ok()
                .and_then(|i| i.framebuffer())
                .and_then(|fb| self.scan_fbs.iter().position(|&f| f == fb))
            {
                blocked |= 1 << i;
            }
            if let Some(i) = self.flipped_parity {
                blocked |= 1 << i;
            }
        }
        // The backend produces the packed BGRA frame — vulkan when
        // available, gles next, cpu last. In direct-scanout mode
        // the whole readback becomes the framebuffer, so the
        // drain must cover the full frame — a clip would leave
        // stale regions visible on the CRTC.
        let spec = crate::renderer::OutputSpec {
            w,
            h,
            ox: self.origin.0,
            oy: self.origin.1,
            name: self.name.clone(),
        };
        let cover_arg = if self.scan_fbs.len() == 3 {
            None
        } else {
            cover_clip.as_deref()
        };
        let t_render = std::time::Instant::now();
        // Cooperative tick for the backend's long phases — drains
        // input and moves the cursor plane mid-render so pointer
        // latency stays bounded by a few ms, not the whole pass.
        // Captures only the fields it needs, disjoint from
        // `self.backend`.
        let hwc = &mut self.hwcursor;
        let card = &self.card;
        let name = &self.name;
        let crtc = self.crtc;
        let origin = self.origin;
        let mode_size = self.mode.size();
        let mut tick = |c: &mut crate::compositor::CompState| {
            let _ = super::input::drain(input, c);
            super::hwcursor::move_if_active(card, crtc, hwc, c, origin, mode_size, name);
        };
        let px_ptr =
            match self
                .backend
                .render(comp, spec.clone(), cover_arg, cover_seq, blocked, &mut tick)
            {
                Ok(p) => {
                    self.render_errors = 0;
                    p.as_ptr()
                }
                Err(err) => {
                    self.render_errors += 1;
                    // A renderer that fails repeatedly is demoted to
                    // the CPU path — a dead GPU must never wedge the
                    // session on a black screen.
                    if self.render_errors >= 3 {
                        tracing::error!(
                            ?err,
                            output = %self.name,
                            "backend failing — demoting to cpu renderer"
                        );
                        self.backend =
                            Box::new(crate::renderer::cpu::CpuBackend::new(spec.clone()));
                        self.render_errors = 0;
                        match self.backend.render(
                            comp,
                            spec.clone(),
                            cover_arg,
                            cover_seq,
                            blocked,
                            &mut tick,
                        ) {
                            Ok(p) => p.as_ptr(),
                            Err(err) => {
                                tracing::warn!(?err, "cpu render failed");
                                requeue!();
                                return;
                            }
                        }
                    } else {
                        tracing::warn!(?err, output = %self.name, "backend render failed");
                        requeue!();
                        return;
                    }
                }
            };
        comp.stat_render_ms = comp
            .stat_render_ms
            .max(t_render.elapsed().as_secs_f64() * 1000.0);
        // Stage the present — `frame_present()` completes it after
        // the event loop's interleaved input drain. The direct-
        // scanout fb for the retired parity rides along so the
        // present can flip instead of memcpy.
        let staged_fb = self
            .backend
            .last_retired()
            .and_then(|i| self.scan_fbs.get(i).copied());
        // In direct-scanout mode the returned slice is a stale
        // readback (the drain is skipped) — the real pixels live
        // in the retired parity's dumb BO map. That pointer feeds
        // dump/screencopy; presents flip and never read it.
        let px_ptr = self
            .backend
            .last_retired()
            .and_then(|i| self.scan_maps.get(i).copied())
            .map(|p| p as *const u8)
            .unwrap_or(px_ptr);
        self.staged = Some(StagedPresent {
            px_ptr,
            clip,
            fb: staged_fb,
        });
    }

    /// Finish a staged frame — copy the pixels into scanout and
    /// present. Runs after the event loop's mid-tick input drain
    /// so pointer events wait at most one render pass, not the
    /// whole render+copy+flip sequence. `input` feeds the same
    /// cooperative yield inside long scanout copies.
    pub fn frame_present(
        &mut self,
        comp: &mut crate::compositor::CompState,
        input: &mut input::Libinput,
    ) {
        let Some(st) = self.staged.take() else {
            return;
        };
        let t_present = std::time::Instant::now();
        self.present(st.px_ptr, st.clip.as_deref(), comp, input, st.fb);
        comp.stat_present_ms = comp
            .stat_present_ms
            .max(t_present.elapsed().as_secs_f64() * 1000.0);
    }
}
