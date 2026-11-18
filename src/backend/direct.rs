// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client-buffer direct scanout — a fullscreen surface whose
//! buffer covers the output 1:1 flips its own dmabuf to the
//! CRTC instead of being composited first. Render pass,
//! readback and dumb-copy all disappear: zero-copy presents
//! for games and video players.
//!
//! This is the standard fast path whenever a single surface
//! occludes an output. The kernel holds the only reference the
//! CRTC needs — imported via `prime_fd_to_buffer` into our
//! card's GEM namespace, wrapped as a planar framebuffer.

use drm::buffer::PlanarBuffer as _;
use drm::control::Device as ControlDevice;
use std::os::fd::AsRawFd;
use wayland_server::backend::ObjectId;
use wayland_server::Resource;

use super::render::Output;
use crate::compositor::BufInfo;

/// `DRM_FORMAT_MOD_LINEAR` — the kernel-level constant; kept
/// local so this file does not reach into the dmabuf module's
/// private constants.
const MOD_LINEAR: u64 = 0;

/// One client dmabuf imported as a scanout framebuffer —
/// `gem` keeps the BO alive in our card namespace and must be
/// closed after the fb is destroyed.
pub(crate) struct ClientFb {
    /// wl_buffer object id — cache key, matches `WlBuffer::id`.
    buf_id: ObjectId,
    gem: drm::buffer::Handle,
    fb: drm::control::framebuffer::Handle,
}

/// `drm::buffer::PlanarBuffer` over an imported client GEM
/// handle — carries the geometry/modifier the client sent in
/// its `zwp_linux_buffer_params` so `add_planar_framebuffer`
/// can express tiled layouts via ADDFB2 modifiers.
struct ClientPlane {
    gem: drm::buffer::Handle,
    w: u32,
    h: u32,
    stride: u32,
    offset: u32,
    format: drm::buffer::DrmFourcc,
    modifier: u64,
}

impl drm::buffer::Buffer for ClientPlane {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> drm::buffer::DrmFourcc {
        self.format
    }
    fn pitch(&self) -> u32 {
        self.stride
    }
    fn handle(&self) -> drm::buffer::Handle {
        self.gem
    }
}

impl drm::buffer::PlanarBuffer for ClientPlane {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> drm::buffer::DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        // LINEAR needs no modifiers flag — the kernel treats an
        // unflagged ADDFB2 as implicit linear anyway, and drivers
        // without IN_FORMATS reject the flag outright.
        if self.modifier == MOD_LINEAR {
            None
        } else {
            Some(drm::buffer::DrmModifier::from(self.modifier))
        }
    }
    fn pitches(&self) -> [u32; 4] {
        [self.stride, 0, 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.gem), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

impl Output {
    /// Try to present the lone fullscreen surface by flipping
    /// its dmabuf straight to the CRTC. Returns true when the
    /// frame is handled — the caller skips the whole composite
    /// path. `false` means composite normally (no candidate,
    /// a shm buffer, or an import/flip failure).
    ///
    /// The same-buffer fast path needs no new flip: the CRTC
    /// already scans that memory, so an in-place repaint is
    /// visible at the next vblank — only frame_done runs, to
    /// pace the client.
    pub fn direct_frame(
        &mut self,
        comp: &mut crate::compositor::CompState,
        _input: &mut input::Libinput,
    ) -> bool {
        if self.direct_dead {
            return false;
        }
        // The software cursor composites into the frame — direct
        // scanout would freeze it, so while the pointer sits on
        // THIS output the hardware plane must own it. A pointer
        // living on another output needs nothing here.
        let (mw0, mh0) = self.mode.size();
        let (px, py) = (comp.ptr_pos.0 as i32, comp.ptr_pos.1 as i32);
        let cursor_here = px >= self.origin.0
            && px < self.origin.0 + mw0 as i32
            && py >= self.origin.1
            && py < self.origin.1 + mh0 as i32;
        if cursor_here && !self.hwcursor.active {
            return false;
        }
        // A drag icon composites outside `scene()` — it must
        // not vanish under a scanned-out buffer.
        if comp.drag.as_ref().is_some_and(|d| d.icon.is_some()) {
            return false;
        }
        let (mw, mh) = self.mode.size();
        let (w, h) = (mw as u32, mh as u32);
        let cand = comp.direct_scan_candidate(self.origin.0, self.origin.1, w as i32, h as i32);
        let Some(surf) = cand else {
            // Left direct mode — the composited BOs are stale
            // now (they hold the last composited frame, missing
            // every direct-scanned update), so the next
            // composite must repaint in full.
            if self.direct_buf.is_some() {
                self.direct_buf = None;
                self.damage_seq += 1;
                self.pending_damage.push_back((self.damage_seq, None));
            }
            return false;
        };
        // Extract the dmabuf descriptor + buffer identity from
        // the surface's shown buffer.
        let (buf_id, dma) = {
            let sd = match surf.data::<crate::compositor::SurfaceData>() {
                Some(sd) => sd,
                None => return false,
            };
            let shown = sd.shown_buf.lock().unwrap();
            let Some(buf) = shown.as_ref() else {
                return false;
            };
            let Some(bi) = buf.data::<BufInfo>() else {
                return false;
            };
            let Some(dma) = bi.dma.as_ref() else {
                return false;
            };
            (
                buf.id(),
                (
                    dma.fd.as_raw_fd(),
                    dma.offset,
                    dma.stride,
                    dma.modifier,
                    bi.format,
                ),
            )
        };
        // Same buffer on the CRTC: in-place damage already
        // reaches the screen — just pace the client.
        if self.direct_buf.as_ref() == Some(&buf_id) {
            let _ = comp.present_damage();
            comp.frame_done(&self.name);
            return true;
        }
        // Import cache lookup — games triple-buffer, so keep a
        // handful of entries and let eviction handle churn.
        let fb = match self.client_fb(buf_id.clone(), dma, w, h) {
            Some(fb) => fb,
            None => return false,
        };
        // Consume this sweep's damage bookkeeping — the whole
        // client buffer presents, so per-output damage is
        // irrelevant to the direct path but must still drain.
        let _ = comp.present_damage();
        self.pending_damage.clear();
        if !self.flip_or_modeset(fb, comp) {
            // Flip rejected — the modifier/layout is not
            // scannable. Compositing still renders correctly,
            // so mark the path dead and fall through.
            self.direct_dead = true;
            self.direct_buf = None;
            return false;
        }
        self.direct_buf = Some(buf_id.clone());
        comp.frame_done(&self.name);
        true
    }

    /// Return the framebuffer handle for a client dmabuf,
    /// importing it on first use. Keeps `direct_fbs` bounded —
    /// entries that left the cache are destroyed except the fb
    /// the CRTC may still be scanning (`last_fb`).
    fn client_fb(
        &mut self,
        buf_id: ObjectId,
        dma: (i32, u64, u32, u64, u32),
        w: u32,
        h: u32,
    ) -> Option<drm::control::framebuffer::Handle> {
        if let Some(e) = self.direct_fbs.iter().find(|e| e.buf_id == buf_id) {
            return Some(e.fb);
        }
        let (fd, offset, stride, modifier, fourcc_u32) = dma;
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let gem = match self.card.prime_fd_to_buffer(borrowed) {
            Ok(g) => g,
            Err(err) => {
                tracing::warn!(?err, "client dmabuf: prime import failed");
                self.direct_dead = true;
                return None;
            }
        };
        let fourcc = match drm::buffer::DrmFourcc::try_from(fourcc_u32) {
            Ok(f) => f,
            Err(_) => {
                let _ = self.card.close_buffer(gem);
                return None;
            }
        };
        let plane = ClientPlane {
            gem,
            w,
            h,
            stride,
            offset: offset as u32,
            format: fourcc,
            modifier,
        };
        let flags = if plane.modifier().is_some() {
            drm::control::FbCmd2Flags::MODIFIERS
        } else {
            drm::control::FbCmd2Flags::empty()
        };
        let fb = match self.card.add_planar_framebuffer(&plane, flags) {
            Ok(f) => f,
            Err(err) => {
                tracing::warn!(?err, modifier, "client dmabuf: addfb2 failed");
                let _ = self.card.close_buffer(gem);
                self.direct_dead = true;
                return None;
            }
        };
        self.direct_fbs.push(ClientFb { buf_id, gem, fb });
        // Bound the cache — evict entries the CRTC can no
        // longer be scanning (never `last_fb`).
        const MAX: usize = 4;
        while self.direct_fbs.len() > MAX {
            let i = self
                .direct_fbs
                .iter()
                .position(|e| Some(e.fb) != self.last_fb)
                .unwrap_or(0);
            let e = self.direct_fbs.remove(i);
            if Some(e.fb) == self.last_fb {
                // Couldn't evict safely — put it back and stop.
                self.direct_fbs.insert(0, e);
                break;
            }
            let _ = self.card.destroy_framebuffer(e.fb);
            let _ = self.card.close_buffer(e.gem);
        }
        Some(fb)
    }
}

/// Tear down every imported client framebuffer — called when
/// the output is dropped or rebuilt so no foreign GEM handle
/// leaks into the next modeset.
impl Output {
    pub(crate) fn clear_direct_fbs(&mut self) {
        for e in self.direct_fbs.drain(..) {
            if Some(e.fb) == self.last_fb {
                continue; // still possibly scanned — leak beats a blank CRTC
            }
            let _ = self.card.destroy_framebuffer(e.fb);
            let _ = self.card.close_buffer(e.gem);
        }
        self.direct_buf = None;
    }
}
