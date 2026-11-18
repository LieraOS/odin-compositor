// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Hardware cursor plane — the DRM cursor ioctls that keep
//! pointer motion completely off the render pipeline.
//!
//! A composited software cursor couples every pointer event to a
//! full scene render + readback + flip; on a pipelined backend
//! the pointer then trails the hardware by a frame or more —
//! the "swimming cursor" users feel whenever a window is open.
//! The dedicated cursor plane decouples it: `move_cursor` is a
//! single ioctl and the scene renders only for real content
//! damage — the same split every production compositor uses.
//!
//! These are the legacy-KMS `DRM_IOCTL_MODE_CURSOR2` ioctls —
//! deprecated upstream in favour of atomic cursor planes but
//! implemented by every driver on the legacy path we already
//! run. A driver without a cursor plane fails `set_cursor2`
//! once and the output permanently falls back to the
//! software-composited cursor — a capability probe, not a
//! policy branch.

use drm::buffer::Buffer as _;
use drm::control::Device as ControlDevice;
use drm::Device as _;
use tracing::info;
use wayland_server::Resource;

use super::render::Output;
use crate::compositor::{BufInfo, CompState, SurfaceData};

/// Per-CRTC cursor plane state — the BO, its persistent map and
/// what the driver currently shows.
pub struct HwCursor {
    /// Cursor BO — the driver-advertised cap size (usually
    /// 64×64), created lazily on first sync so a failed probe
    /// costs nothing.
    db: Option<drm::control::dumbbuffer::DumbBuffer>,
    /// Persistent dumb-buffer map — same rationale as the
    /// scanout map: per-update map ioctls churn mmap state.
    map_ptr: *mut u8,
    map_pitch: usize,
    /// BO dimensions the driver advertised.
    size: (u32, u32),
    /// The driver owns our pointer — scene composites skip the
    /// cursor on this output entirely.
    pub active: bool,
    /// Plane probing failed (no support) — never retry. EACCES
    /// during a VT switch does NOT set this: master loss is
    /// transient, missing hardware is not.
    dead: bool,
    /// `comp.cursor_gen` value the BO currently holds — a
    /// changed image re-uploads, a steady one never touches
    /// the cursor BO.
    gen: u64,
    /// The CRTC is showing our cursor right now.
    shown: bool,
}

impl HwCursor {
    pub fn new() -> Self {
        Self {
            db: None,
            map_ptr: std::ptr::null_mut(),
            map_pitch: 0,
            size: (0, 0),
            active: false,
            dead: false,
            gen: 0,
            shown: false,
        }
    }
}

/// Snapshot the image the software path would draw — themed
/// Xcursor first, then the client's cursor surface, finally the
/// built-in arrow. Returns packed BGRA pixels plus hotspot.
fn cursor_image(comp: &CompState) -> (Vec<u8>, i32, i32, i32, i32) {
    if let Some(img) = &comp.cursor_img {
        return (
            img.pixels.clone(),
            img.w as i32,
            img.h as i32,
            img.hx,
            img.hy,
        );
    }
    if let Some(s) = comp.cursor_surf.as_ref().filter(|s| s.is_alive()) {
        if let Some(sd) = s.data::<SurfaceData>() {
            let shown = sd.shown_buf.lock().unwrap();
            if let Some((bi, base)) = shown
                .as_ref()
                .and_then(|b| b.data::<BufInfo>().map(|bi| (bi, b)))
            {
                let map = bi.map.lock().unwrap();
                // A failed pool mmap (MAP_FAILED) or geometry
                // reaching past the map end must not feed the
                // raw copy below — buf geometry is validated at
                // create time, but the map itself can still be
                // the failure sentinel.
                let mut out = vec![0u8; (bi.w * bi.h * 4).max(0) as usize];
                let row = (bi.w * 4) as usize;
                if !map.ptr.is_null() && map.ptr as isize != -1 {
                    for y in 0..bi.h as usize {
                        let so = bi.offset as usize + y * bi.stride as usize;
                        // Clamp the source row to the live map —
                        // a pool resize can move the end under a
                        // buffer created before it.
                        if so + row > map.len {
                            break;
                        }
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                map.ptr.add(so),
                                out.as_mut_ptr().add(y * row),
                                row,
                            );
                        }
                    }
                }
                let _ = base;
                return (
                    out,
                    bi.w,
                    bi.h,
                    comp.cursor_hotspot.0,
                    comp.cursor_hotspot.1,
                );
            }
        }
    }
    // Built-in arrow — the same glyph the CPU composite draws.
    let mut px = vec![0u8; 12 * 19 * 4];
    crate::cursor::draw(&mut px, 12, 19, 0, 0);
    (px, 12, 19, 0, 0)
}

/// Move-only cursor sync for mid-render yields — when the
/// plane already owns this output's pointer, keep its position
/// live even inside a long frame pass instead of letting it
/// freeze until the loop reaches `sync_cursor` again. Arming,
/// image upload, hiding and plane-death stay in `sync_cursor`:
/// out-of-bounds positions are skipped (the next full sync
/// hides the plane), EACCES is tolerated like the main path.
pub(crate) fn move_if_active(
    card: &super::render::Card,
    crtc: drm::control::crtc::Handle,
    hwc: &mut HwCursor,
    comp: &mut CompState,
    origin: (i32, i32),
    mode_size: (u16, u16),
    name: &str,
) {
    if !hwc.active || hwc.dead {
        return;
    }
    let (mw, mh) = (mode_size.0 as i32, mode_size.1 as i32);
    let lx = comp.ptr_pos.0 as i32 - origin.0;
    let ly = comp.ptr_pos.1 as i32 - origin.1;
    if lx < 0 || lx >= mw || ly < 0 || ly >= mh {
        return;
    }
    #[allow(deprecated)]
    if let Err(err) = card.move_cursor(crtc, (lx, ly)) {
        if err.raw_os_error() != Some(libc::EACCES) {
            // Same teardown as `cursor_plane_dead`: flag the
            // output back to the software cursor and queue the
            // repaint — `sync_cursor` early-returns on `dead`,
            // so without this damage the pointer would go
            // invisible until unrelated scene damage arrives.
            hwc.dead = true;
            hwc.active = false;
            comp.hw_cursor.remove(name);
            comp.sw_cursor = true;
            comp.damage.push((origin.0, origin.1, mw, mh));
            comp.dirty = true;
        }
    }
}

impl Output {
    /// Drive the CRTC cursor to the pointer's state — re-upload
    /// the image on a generation change, move it every call,
    /// hide it while the pointer sits outside this output.
    /// Called once per input batch and per frame tick; every
    /// branch except the move ioctl is a no-op in steady state.
    pub fn sync_cursor(&mut self, comp: &mut CompState) {
        if self.hwcursor.dead {
            return;
        }
        let (mw, mh) = self.mode.size();
        let (mw, mh) = (mw as i32, mh as i32);
        let lx = comp.ptr_pos.0 as i32 - self.origin.0;
        let ly = comp.ptr_pos.1 as i32 - self.origin.1;
        if lx < 0 || lx >= mw || ly < 0 || ly >= mh {
            // Pointer lives on another output — hide rather than
            // park a stale image at the edge.
            if self.hwcursor.shown {
                #[allow(deprecated)]
                let _ = self
                    .card
                    .set_cursor2::<drm::control::dumbbuffer::DumbBuffer>(self.crtc, None, (0, 0));
                self.hwcursor.shown = false;
            }
            return;
        }
        if !self.ensure_cursor_buf() {
            self.cursor_plane_dead(comp);
            return;
        }
        if self.hwcursor.gen != comp.cursor_gen || !self.hwcursor.shown {
            let (px, w, h, hx, hy) = cursor_image(comp);
            if let Err(err) = self.upload_cursor(&px, w, h, hx, hy) {
                if err.raw_os_error() == Some(libc::EACCES) {
                    return; // transient master loss — retry next sync
                }
                self.cursor_plane_dead(comp);
                return;
            }
            self.hwcursor.gen = comp.cursor_gen;
        }
        #[allow(deprecated)]
        if let Err(err) = self.card.move_cursor(self.crtc, (lx, ly)) {
            if err.raw_os_error() != Some(libc::EACCES) {
                self.cursor_plane_dead(comp);
            }
            return;
        }
        self.hwcursor.shown = true;
        if !self.hwcursor.active {
            self.hwcursor.active = true;
            comp.hw_cursor.insert(self.name.clone());
            // The last software-composited frame still carries
            // cursor pixels wherever the pointer stood — repaint
            // this output once so the plane handoff leaves no
            // stale image behind.
            comp.damage.push((self.origin.0, self.origin.1, mw, mh));
            comp.dirty = true;
            info!(output = %self.name, "hardware cursor plane active");
        }
    }

    /// The driver rejected the cursor plane — drop every handle
    /// and flag the output back to the composited cursor so the
    /// renderers and the motion-damage path resume drawing it.
    fn cursor_plane_dead(&mut self, comp: &mut CompState) {
        self.hwcursor.dead = true;
        self.hwcursor.active = false;
        comp.hw_cursor.remove(&self.name);
        comp.sw_cursor = true;
        // Back to composited pixels — the output needs a frame
        // so the software cursor becomes visible again.
        let (mw, mh) = self.mode.size();
        comp.damage
            .push((self.origin.0, self.origin.1, mw as i32, mh as i32));
        comp.dirty = true;
        info!(output = %self.name, "no cursor plane — software cursor");
    }

    /// Lazily create the cursor BO at the driver's advertised
    /// size — the cap read fails only on drivers without a
    /// cursor plane, which is exactly the fallback signal.
    fn ensure_cursor_buf(&mut self) -> bool {
        if self.hwcursor.db.is_some() {
            return true;
        }
        let w = self
            .card
            .get_driver_capability(drm::DriverCapability::CursorWidth)
            .unwrap_or(64)
            .clamp(16, 256) as u32;
        let h = self
            .card
            .get_driver_capability(drm::DriverCapability::CursorHeight)
            .unwrap_or(64)
            .clamp(16, 256) as u32;
        let mut db =
            match self
                .card
                .create_dumb_buffer((w, h), drm::buffer::DrmFourcc::Argb8888, 32)
            {
                Ok(db) => db,
                Err(_) => return false,
            };
        let pitch = db.pitch() as usize;
        match self.card.map_dumb_buffer(&mut db) {
            Ok(mut map) => {
                self.hwcursor.map_ptr = map.as_mut().as_mut_ptr();
                self.hwcursor.map_pitch = pitch;
                // Same deliberate leak as the scanout map — the
                // mapping outlives the Buffer's borrow.
                std::mem::forget(map);
            }
            Err(_) => return false,
        }
        self.hwcursor.size = (w, h);
        self.hwcursor.db = Some(db);
        true
    }

    /// Write the BGRA image into the cursor BO and arm the plane
    /// with its hotspot — one clear + row-copy + ioctl.
    fn upload_cursor(
        &mut self,
        px: &[u8],
        w: i32,
        h: i32,
        hx: i32,
        hy: i32,
    ) -> std::io::Result<()> {
        let (cw, ch) = self.hwcursor.size;
        let (cw, ch) = (cw as usize, ch as usize);
        let dst = unsafe {
            std::slice::from_raw_parts_mut(self.hwcursor.map_ptr, self.hwcursor.map_pitch * ch)
        };
        dst.fill(0);
        let rows = (h as usize).min(ch);
        let cols = (w as usize).min(cw);
        for y in 0..rows {
            let s = y * w.max(0) as usize * 4;
            let d = y * self.hwcursor.map_pitch;
            let n = cols * 4;
            if s + n <= px.len() && d + n <= dst.len() {
                dst[d..d + n].copy_from_slice(&px[s..s + n]);
            }
        }
        let hx = hx.clamp(0, w - 1).max(0);
        let hy = hy.clamp(0, h - 1).max(0);
        #[allow(deprecated)]
        self.card
            .set_cursor2(self.crtc, self.hwcursor.db.as_ref(), (hx, hy))
    }
}
