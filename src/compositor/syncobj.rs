// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wp_linux_drm_syncobj_v1 — explicit synchronization for
//! dmabuf clients (games, video players, Vulkan stacks).
//!
//! The protocol attaches acquire/release timeline points to a
//! wl_surface's double-buffered state. At commit we may not
//! sample the new buffer until the acquire point's fence
//! signaled, and we must signal the release point once the
//! compositor is done with that buffer instance.
//!
//! Two key decisions:
//!
//! * Every syncobj ioctl runs on the CLIENT's dup'd fd — the
//!   fd carries its own device context, so no access to our
//!   render node is needed (works identically on nested).
//!
//! * Instead of eventfd plumbing, the frame tick polls waiters
//!   non-blockingly (`timeline_wait` timeout 0 / `poll` on the
//!   dmabuf fd). A fence that signals between ticks lands in
//!   the very next frame — the same latency damage-driven
//!   repaints already have.
//!
//! Implicit sync rides the same machinery: a dmabuf attach on
//! a surface WITHOUT a syncobj object polls the dma fd with
//! POLLIN — not signaled means the attach is stashed, fixing
//! the sample-early flicker class without client cooperation.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use drm::control::syncobj::Handle as SyncobjHandle;
use drm::control::Device as ControlDevice;
use wayland_protocols::wp::linux_drm_syncobj::v1::server::{
    wp_linux_drm_syncobj_manager_v1 as so_mgr, wp_linux_drm_syncobj_surface_v1 as so_surf,
    wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    wp_linux_drm_syncobj_timeline_v1 as so_tl,
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
};
use wayland_server::protocol::{wl_buffer, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

pub use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1;

/// Adapter turning a raw fd into a `drm::Device` — the same
/// trick backend/drm.rs uses for the session card. Syncobj
/// ioctls on a syncobj fd operate on the device the fd came
/// from, so a dup of the client's fd is self-sufficient.
struct DevFd<'a>(BorrowedFd<'a>);
impl AsFd for DevFd<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0
    }
}
impl drm::Device for DevFd<'_> {}
impl drm::control::Device for DevFd<'_> {}

/// An imported client syncobj: a dup of OUR canonical device fd
/// plus the handle `fd_to_handle` minted in its table. The dup
/// keeps that file description (and with it the handle's
/// validity) alive independent of the protocol object's fate.
pub struct SyncTl {
    pub fd: OwnedFd,
    pub handle: SyncobjHandle,
}

/// Open the first usable DRM render node — syncobj ioctls
/// belong on a real device file, and NVIDIA rejects
/// `fd_to_handle` issued on the exported syncobj fd itself.
/// Render nodes are world-accessible, so this works under
/// nested and headless backends too.
fn open_render_node() -> Option<OwnedFd> {
    (128..144).find_map(|n| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/dri/renderD{n}"))
            .ok()
            .map(Into::into)
    })
}

/// A timeline point ready for ioctl use — fd + handle + point.
/// Cloning dups the fd so pending state survives the client's
/// `timeline.destroy` (the object may die; the point may not).
pub struct SyncPoint {
    pub fd: OwnedFd,
    pub handle: SyncobjHandle,
    pub point: u64,
}

impl SyncPoint {
    /// Materialize a handle+fd pair out of a timeline object.
    /// dup keeps the file description (and its handle table)
    /// alive independently of the protocol object's lifetime.
    fn of(tl: &WpLinuxDrmSyncobjTimelineV1, hi: u32, lo: u32) -> Option<Self> {
        let t = tl.data::<SyncTl>()?;
        let fd = rustix::io::dup(&t.fd).ok()?;
        Some(Self {
            fd,
            handle: t.handle,
            point: ((hi as u64) << 32) | lo as u64,
        })
    }

    /// Same kernel syncobj? Every import lands in the canonical
    /// device fd's handle table, where the driver dedups repeated
    /// imports of one object — so equal handles mean one timeline,
    /// which is exactly what `conflicting_points` guards.
    pub(crate) fn same_timeline(&self, other: &Self) -> bool {
        self.handle == other.handle
    }

    /// Non-blocking check — is this point's fence signaled?
    /// `Ok(true)` ready, `Ok(false)` still in flight, `Err`
    /// the fd/handle died (callers treat as ready-fail:
    /// never hold a buffer hostage to a broken fence).
    pub fn signaled(&self) -> bool {
        DevFd(self.fd.as_fd())
            .syncobj_timeline_wait(&[self.handle], &[self.point], 0, false, false, false)
            .is_ok()
    }

    /// Signal the point — our side telling the client the
    /// buffer is free to reuse.
    pub fn signal(&self) {
        let _ = DevFd(self.fd.as_fd()).syncobj_timeline_signal(&[self.handle], &[self.point]);
    }
}

/// What a stashed attach waits on before it may become shown.
pub enum SyncWait {
    /// Explicit sync — a client syncobj timeline point.
    Tl(SyncPoint),
    /// Implicit sync — the dmabuf's own fences, polled on the
    /// plane fd kept by `BufInfo::dma` (borrowed at check time;
    /// the stashed buffer keeps it alive).
    Dma,
}

/// A committed attach held back until its fence signals — the
/// buffer plus its wait, and the release point that activates
/// the moment it lands on-screen.
pub struct SyncStash {
    pub buf: wl_buffer::WlBuffer,
    pub wait: SyncWait,
    pub rel: Option<SyncPoint>,
}

/// Manager dispatch — object factory for timelines and the
/// per-surface sync extension.
impl Dispatch<WpLinuxDrmSyncobjManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        mgr: &WpLinuxDrmSyncobjManagerV1,
        request: so_mgr::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            so_mgr::Request::ImportTimeline { id, fd } => {
                // The canonical device fd is resolved lazily —
                // the real backend sets it at output init, nested
                // discovers the render node on first import.
                if state.drm_fd.is_none() {
                    state.drm_fd = open_render_node();
                }
                // A dead timeline for error paths — the object must
                // exist or the backend panics on the unborn id and
                // kills the whole compositor, not just the client.
                let dead_tl = || SyncTl {
                    fd: std::fs::File::open("/dev/null")
                        .expect("open /dev/null")
                        .into(),
                    handle: std::num::NonZeroU32::new(1).unwrap().into(),
                };
                let Some(dev) = state.drm_fd.as_ref().map(|f| f.as_fd()) else {
                    init.init::<WpLinuxDrmSyncobjTimelineV1, SyncTl>(id, dead_tl());
                    dh.post_error(
                        mgr,
                        so_mgr::Error::InvalidTimeline as u32,
                        "no drm device to import timelines on".to_string(),
                    );
                    return;
                };
                // fd_to_handle must run on a proper device file —
                // NVIDIA rejects the same ioctl on the client's
                // exported syncobj fd (what killed XWayland).
                let Ok(handle) = DevFd(dev).fd_to_syncobj(fd.as_fd(), false) else {
                    init.init::<WpLinuxDrmSyncobjTimelineV1, SyncTl>(id, dead_tl());
                    dh.post_error(
                        mgr,
                        so_mgr::Error::InvalidTimeline as u32,
                        "fd is not a drm_syncobj".to_string(),
                    );
                    return;
                };
                // The dup shares the canonical fd's file
                // description — the imported handle stays valid
                // on it even if the canonical fd is later swapped.
                let dev_dup = rustix::io::dup(dev).expect("dup drm device fd");
                init.init::<WpLinuxDrmSyncobjTimelineV1, SyncTl>(
                    id,
                    SyncTl {
                        fd: dev_dup,
                        handle,
                    },
                );
            }
            so_mgr::Request::GetSurface { id, surface } => {
                // The id must be born before any error return —
                // an uninit'd object panics the wayland backend
                // and kills the compositor, not just the client.
                let obj = init
                    .init::<WpLinuxDrmSyncobjSurfaceV1, wl_surface::WlSurface>(id, surface.clone());
                // One sync extension per wl_surface — a second
                // get_surface is the spec's surface_exists error.
                let Some(sd) = surface.data::<SurfaceData>() else {
                    dh.post_error(
                        mgr,
                        so_mgr::Error::InvalidTimeline as u32,
                        "unknown wl_surface".to_string(),
                    );
                    return;
                };
                if sd.syncobj_surf.lock().unwrap().is_some() {
                    dh.post_error(
                        mgr,
                        so_mgr::Error::SurfaceExists as u32,
                        "wl_surface already has a syncobj extension".to_string(),
                    );
                    return;
                }
                *sd.syncobj_surf.lock().unwrap() = Some(obj);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpLinuxDrmSyncobjTimelineV1, SyncTl> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpLinuxDrmSyncobjTimelineV1,
        _: so_tl::Request,
        _: &SyncTl,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` exists — auto-destructor.
    }

    /// Client freed the object (or disconnected) — our imported
    /// handle goes back to the kernel; the client's own timeline
    /// is untouched, the import only held a reference.
    fn destroyed(
        _: &mut Self,
        _: wayland_server::backend::ClientId,
        _: &WpLinuxDrmSyncobjTimelineV1,
        data: &SyncTl,
    ) {
        let _ = DevFd(data.fd.as_fd()).destroy_syncobj(data.handle);
    }
}

impl Dispatch<WpLinuxDrmSyncobjSurfaceV1, wl_surface::WlSurface> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpLinuxDrmSyncobjSurfaceV1,
        request: so_surf::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        match request {
            so_surf::Request::SetAcquirePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                // Double-buffered: replaces any pending point —
                // the last one set before commit wins.
                if let Some(pt) = SyncPoint::of(&timeline, point_hi, point_lo) {
                    *sd.sync_acq.lock().unwrap() = Some(pt);
                }
            }
            so_surf::Request::SetReleasePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                if let Some(pt) = SyncPoint::of(&timeline, point_hi, point_lo) {
                    *sd.sync_rel.lock().unwrap() = Some(pt);
                }
            }
            so_surf::Request::Destroy => {
                // After destroy the surface falls back to the
                // implicit path — pending points are dropped.
                *sd.syncobj_surf.lock().unwrap() = None;
                *sd.sync_acq.lock().unwrap() = None;
                *sd.sync_rel.lock().unwrap() = None;
            }
            _ => {}
        }
    }
}
