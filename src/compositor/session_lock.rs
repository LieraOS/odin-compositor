// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_session_lock_v1 — session lock surfaces.
//!
//! The screenlock client asks for a lock, gets one exclusive
//! `ext_session_lock_v1`, and then supplies one lock surface per
//! output. While `CompState::lock` is set the scene renders ONLY
//! those surfaces (plus the cursor) and `hit()` reports nothing
//! else — normal windows, panels and popups are unreachable for
//! input and invisible on screen. `unlock_and_destroy` finishes
//! the lock and restores the desktop; a dead lock client drops
//! the lock automatically on the next session_tick.
//!
//! Escape hatch by design: the `quit` keybinding still fires
//! while locked — a wedged screenlock must never force a reboot.

use tracing::{info, warn};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1 as esm, ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1 as ess, ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    ext_session_lock_v1 as esl, ext_session_lock_v1::ExtSessionLockV1,
};
use wayland_server::protocol::{wl_output, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::ExtSessionLockManagerV1 as ExtSessionLockManager;

/// One lock surface and the configure serial it must ack.
struct LockSurface {
    surf: wl_surface::WlSurface,
    obj: ExtSessionLockSurfaceV1,
    /// Output name from the wl_output's global data — the lock
    /// surface fills exactly that output.
    output: String,
    /// Last configure serial sent — ack_configure must match.
    pending_serial: u32,
    /// First ack landed — commits before it are a protocol error.
    acked: bool,
}

/// The live session lock — its presence IS the locked state.
pub struct SessionLock {
    obj: ExtSessionLockV1,
    surfaces: Vec<LockSurface>,
}

impl CompState {
    /// Session-lock state — named distinctly from `locked()`,
    /// which belongs to pointer constraints.
    pub fn session_locked(&self) -> bool {
        self.lock.is_some()
    }

    /// Lock surfaces for the render path — drawn at their
    /// SurfaceData position (the output's origin, set at
    /// get_lock_surface time).
    pub(crate) fn lock_surfaces(&self) -> Vec<wl_surface::WlSurface> {
        self.lock
            .as_ref()
            .map(|l| l.surfaces.iter().map(|s| s.surf.clone()).collect())
            .unwrap_or_default()
    }

    /// Hit test restricted to lock surfaces — called by `hit()`
    /// while locked so pointer input can't reach anything else.
    pub(crate) fn hit_lock(&self, x: f64, y: f64) -> Option<(wl_surface::WlSurface, f64, f64)> {
        let lock = self.lock.as_ref()?;
        for ls in lock.surfaces.iter().rev() {
            let Some(sd) = ls.surf.data::<SurfaceData>() else {
                continue;
            };
            let (px, py) = *sd.pos.lock().unwrap();
            let (bw, bh) = sd
                .shown_buf
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|b| b.data::<super::BufInfo>())
                .map(|bi| (bi.w, bi.h))
                .unwrap_or((0, 0));
            if x >= px as f64 && x < (px + bw) as f64 && y >= py as f64 && y < (py + bh) as f64 {
                return Some((ls.surf.clone(), x - px as f64, y - py as f64));
            }
        }
        None
    }

    /// Per-tick lock housekeeping — a dead lock object means the
    /// screenlock client crashed; the session must unlock itself
    /// rather than leave the user stranded on a dead surface.
    pub fn lock_tick(&mut self) {
        if self.lock.as_ref().is_some_and(|l| !l.obj.is_alive()) {
            warn!("session lock client died — unlocking session");
            self.end_lock();
        }
    }

    /// Shared teardown for unlock_and_destroy and client death.
    fn end_lock(&mut self) {
        let Some(lock) = self.lock.take() else { return };
        for ls in &lock.surfaces {
            if let Some(sd) = ls.surf.data::<SurfaceData>() {
                sd.is_lock_surface
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.damage_everything();
        // Refocus the topmost real window — the lock client's
        // surfaces are gone, so the stale kb_focus object is dead.
        let top = self.order.last().cloned();
        self.set_kbd_focus(top);
        info!("session unlocked");
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtSessionLockManagerV1,
        request: esm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let esm::Request::Lock { id } = request {
            let obj = init.init::<ExtSessionLockV1, ()>(id, ());
            if state.lock.is_some() {
                // Only one lock at a time — the second client gets
                // an immediate finished and no state.
                obj.finished();
                return;
            }
            info!("session locked");
            obj.locked();
            state.lock = Some(SessionLock {
                obj,
                surfaces: Vec::new(),
            });
            state.damage_everything();
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        res: &ExtSessionLockV1,
        request: esl::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            esl::Request::GetLockSurface {
                id,
                surface,
                output,
            } => state.new_lock_surface(res, id, surface, output, init),
            esl::Request::UnlockAndDestroy => {
                res.finished();
                state.end_lock();
            }
            esl::Request::Destroy => {
                // Legal only before locked() — we send locked()
                // synchronously, so a later destroy is an error.
                res.post_error(
                    esl::Error::InvalidDestroy,
                    "session lock already established",
                );
            }
            _ => {}
        }
    }
}

impl CompState {
    fn new_lock_surface(
        &mut self,
        res: &ExtSessionLockV1,
        id: wayland_server::New<ExtSessionLockSurfaceV1>,
        surface: wl_surface::WlSurface,
        output: wl_output::WlOutput,
        init: &mut DataInit<'_, Self>,
    ) {
        // The new id must be born on every path — an uninit'd
        // object panics the wayland backend and kills the
        // compositor whole, not just the offending client.
        let obj = init.init::<ExtSessionLockSurfaceV1, ()>(id, ());
        // The wl_output's global data is the connector name —
        // resolve it to the live output's rect for configure dims.
        let out_name = output.data::<String>().cloned().unwrap_or_default();
        let rect = self
            .layout
            .outputs
            .get(&out_name)
            .map(|o| (o.mode.0, o.mode.1, o.x, o.y));
        let Some((w, h, ox, oy)) = rect else {
            res.post_error(esl::Error::DuplicateOutput, "unknown output");
            return;
        };
        let Some(sd) = surface.data::<SurfaceData>() else {
            return;
        };
        // Spec gates: a surface with an existing role or an
        // attached buffer can't become a lock surface.
        if Self::has_window_role(sd)
            || sd.is_cursor.load(std::sync::atomic::Ordering::SeqCst)
            || self.layer_index(&surface).is_some()
        {
            res.post_error(esl::Error::Role, "surface already has a role");
            return;
        }
        if sd.shown_buf.lock().unwrap().is_some() {
            res.post_error(esl::Error::AlreadyConstructed, "surface has a buffer");
            return;
        }
        if self
            .lock
            .as_ref()
            .is_some_and(|l| l.surfaces.iter().any(|s| s.output == out_name))
        {
            res.post_error(esl::Error::DuplicateOutput, "output already locked");
            return;
        }
        let serial = self.next_serial_pub();
        let Some(lock) = self.lock.as_mut() else {
            return;
        };
        sd.is_lock_surface
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *sd.pos.lock().unwrap() = (ox, oy);
        obj.configure(serial, w, h);
        lock.surfaces.push(LockSurface {
            surf: surface,
            obj,
            output: out_name,
            pending_serial: serial,
            acked: false,
        });
        info!(w, h, "session lock surface configured");
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        res: &ExtSessionLockSurfaceV1,
        request: ess::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ess::Request::AckConfigure { serial } => {
                let Some(lock) = state.lock.as_mut() else {
                    return;
                };
                let Some(ls) = lock.surfaces.iter_mut().find(|s| s.obj.id() == res.id()) else {
                    return;
                };
                if serial != ls.pending_serial {
                    res.post_error(ess::Error::InvalidSerial, "stale configure serial");
                    return;
                }
                ls.acked = true;
            }
            ess::Request::Destroy => {
                if let Some(lock) = state.lock.as_mut() {
                    lock.surfaces.retain(|s| s.obj.id() != res.id());
                }
            }
            _ => {}
        }
    }
}
