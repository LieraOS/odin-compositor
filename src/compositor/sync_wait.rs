// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Sync-wait machinery — commit gating, stashed-attach
//! processing and deferred release-point signaling.
//!
//! `commit_gate` runs inside every wl_surface.commit before
//! the pending buffer reaches `shown_buf`. Three outcomes:
//!
//!   * pass through — the buffer may be shown immediately;
//!   * stash — hold the buffer aside until its fence signals
//!     (explicit acquire point or implicit dmabuf fences);
//!   * reject — a protocol error on a syncobj surface
//!     (missing/conflicting points); the buffer is dropped.
//!
//! `sync_tick` polls stashes once per frame tick and lands
//! ready buffers on-screen. `signal_releases` drains the
//! deferred release-point queue next to wl_buffer.release —
//! the same two-presented-frames discipline already proven
//! for dmabuf flicker prevention.

use std::os::fd::AsFd;

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_surface_v1 as so_surf;
use wayland_server::protocol::{wl_buffer, wl_surface};
use wayland_server::Resource;

use super::shm::BufInfo;
use super::syncobj::{SyncPoint, SyncStash, SyncWait};
use super::{CompState, SurfaceData};

impl CompState {
    /// Commit-time gate for the pending attach. The outer
    /// Option is "was there an attach this commit", the inner
    /// is the buffer (Some(None) = NULL attach, i.e. unmap).
    /// Returns the attach to install now plus the release point
    /// that becomes the new `release_of`, or `(None, _)` when
    /// the attach was stashed/rejected/absent. Also clears
    /// pending acquire/release state — double-buffered like
    /// the attach itself.
    pub(crate) fn commit_gate(
        &mut self,
        surf: &wl_surface::WlSurface,
        data: &SurfaceData,
        buf: Option<Option<wl_buffer::WlBuffer>>,
    ) -> (Option<Option<wl_buffer::WlBuffer>>, Option<SyncPoint>) {
        let syncobj_on = data.syncobj_surf.lock().unwrap().is_some();
        let acq = data.sync_acq.lock().unwrap().take();
        let rel = data.sync_rel.lock().unwrap().take();
        if syncobj_on {
            let has_buffer = matches!(buf, Some(Some(_)));
            // Explicit-sync validation — mirrors the spec's
            // error table exactly; a violation posts the error
            // and rejects the attach wholesale.
            let err = if !has_buffer && (acq.is_some() || rel.is_some()) {
                Some(so_surf::Error::NoBuffer)
            } else if has_buffer && acq.is_none() {
                Some(so_surf::Error::NoAcquirePoint)
            } else if has_buffer && rel.is_none() {
                Some(so_surf::Error::NoReleasePoint)
            } else if let (Some(a), Some(r)) = (&acq, &rel) {
                if has_buffer && a.same_timeline(r) && a.point >= r.point {
                    Some(so_surf::Error::ConflictingPoints)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(e) = err {
                if let Some(obj) = data.syncobj_surf.lock().unwrap().as_ref() {
                    self.dh.post_error(
                        obj,
                        e as u32,
                        "syncobj commit contract violated".to_string(),
                    );
                }
                // The attach is rejected — never sampled.
                return (None, None);
            }
            match (buf, acq) {
                (Some(Some(b)), Some(a)) => {
                    if a.signaled() {
                        return (Some(Some(b)), rel);
                    }
                    // Fence still in flight — hold the buffer
                    // aside; the tick lands it once the point
                    // signals.
                    self.stash_push(
                        surf,
                        data,
                        SyncStash {
                            buf: b,
                            wait: SyncWait::Tl(a),
                            rel,
                        },
                    );
                    (None, None)
                }
                // No buffer this commit — pass the attach
                // through untouched (None or NULL-attach unmap).
                (other, _) => (other, None),
            }
        } else {
            // No syncobj extension — implicit sync for dmabuf: the
            // kernel fence attached to the dma fd must signal before
            // we sample. POLLIN = read-side (exclusive fence done).
            if let Some(Some(buf)) = buf {
                let dma_ready = buf
                    .data::<BufInfo>()
                    .and_then(|bi| bi.dma.as_ref())
                    .map(|d| {
                        let mut pfd = [PollFd::new(d.fd.as_fd(), PollFlags::POLLIN)];
                        poll(&mut pfd, PollTimeout::ZERO)
                            .map(|n| n > 0 && pfd[0].revents().unwrap().contains(PollFlags::POLLIN))
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if dma_ready {
                    return (Some(Some(buf)), None);
                }
                self.stash_push(
                    surf,
                    data,
                    SyncStash {
                        buf,
                        wait: SyncWait::Dma,
                        rel: None,
                    },
                );
                return (None, None);
            }
            (buf, None)
        }
    }

    /// Install a stash — a superseded one is disposed like a
    /// replaced shown buffer: wl_buffer.release deferred, its
    /// release point queued for signaling (never sampled means
    /// "finished usage" trivially holds).
    fn stash_push(&mut self, surf: &wl_surface::WlSurface, data: &SurfaceData, st: SyncStash) {
        if let Some(old) = data.sync_stash.lock().unwrap().replace(st) {
            if old.buf.is_alive() {
                self.pending_release.push(old.buf);
            }
            if let Some(r) = old.rel {
                self.tsig_pending.push(r);
            }
        }
        if !self.sync_waiters.iter().any(|s| s.id() == surf.id()) {
            self.sync_waiters.push(surf.clone());
        }
    }

    /// Install a stashed buffer as shown — same discipline the
    /// commit path applies: the displaced buffer defers its
    /// wl_buffer.release, its release point joins tsig_pending.
    fn stash_apply(&mut self, surf: &wl_surface::WlSurface, data: &SurfaceData, st: SyncStash) {
        let next_id = st.buf.id();
        let prev = data.shown_buf.lock().unwrap().replace(st.buf);
        if let Some(pt) = data.release_of.lock().unwrap().take() {
            self.tsig_pending.push(pt);
        }
        *data.release_of.lock().unwrap() = st.rel;
        if let Some(old) = prev {
            if old.is_alive() && old.id() != next_id {
                self.pending_release.push(old);
            }
        }
        // Force a full texture upload — the deferred attach must
        // not inherit damage bookkeeping of the frame it missed.
        data.upload_damage.lock().unwrap().2 = None;
        // The new pixels land next composite — mark the surface's
        // whole rect damaged in scene space.
        let (w, h) = self.surf_size(data);
        let (dx, dy) = self.local_origin(surf);
        let (sx, sy) = *data.offset.lock().unwrap();
        self.damage.push((dx as i32 + sx, dy as i32 + sy, w, h));
        self.dirty = true;
    }

    /// Once per frame tick: check every stashed attach. A
    /// signaled fence installs the buffer immediately; a dead
    /// waiter (surface gone, fence object broken) is dropped so
    /// it can never wedge the surface.
    pub(crate) fn sync_tick(&mut self) {
        if self.sync_waiters.is_empty() {
            return;
        }
        let waiters = std::mem::take(&mut self.sync_waiters);
        for surf in waiters {
            if !surf.is_alive() {
                continue;
            }
            let Some(sd) = surf.data::<SurfaceData>() else {
                continue;
            };
            // A newer commit may have superseded the stash —
            // only a live stash participates.
            let ready = {
                let stash = sd.sync_stash.lock().unwrap();
                match stash.as_ref().map(|s| &s.wait) {
                    None => false,
                    Some(SyncWait::Tl(pt)) => pt.signaled(),
                    Some(SyncWait::Dma) => sd
                        .sync_stash
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|s| s.buf.data::<BufInfo>())
                        .and_then(|bi| bi.dma.as_ref())
                        .map(|d| {
                            let mut pfd = [PollFd::new(d.fd.as_fd(), PollFlags::POLLIN)];
                            poll(&mut pfd, PollTimeout::ZERO)
                                .map(|n| {
                                    n > 0 && pfd[0].revents().unwrap().contains(PollFlags::POLLIN)
                                })
                                .unwrap_or(true)
                        })
                        .unwrap_or(true),
                }
            };
            if !ready {
                self.sync_waiters.push(surf);
                continue;
            }
            let st = sd.sync_stash.lock().unwrap().take();
            if let Some(st) = st {
                self.stash_apply(&surf, sd, st);
            }
        }
    }

    /// Called from `frame_done`: deferred release-point
    /// signaling, two presented frames of delay — identical to
    /// the wl_buffer.release discipline below it.
    pub(crate) fn signal_releases(&mut self) {
        for pt in self.tsig_ready.drain(..) {
            pt.signal();
        }
        std::mem::swap(&mut self.tsig_ready, &mut self.tsig_next);
        self.tsig_next.append(&mut self.tsig_pending);
    }
}
