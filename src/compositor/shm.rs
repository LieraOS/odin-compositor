// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_shm — shared-memory pools, buffers and regions.
//!
//! Clients hand us an fd; we mmap it read-only and keep the map
//! behind a Mutex because `resize` remaps in place while the
//! render loop may be mid-blit. All the unsafe lives here — the
//! rest of the compositor only ever sees BufInfo.

use std::os::fd::{AsRawFd, OwnedFd};
use tracing::{info, warn};
use wayland_server::protocol::{wl_buffer, wl_region, wl_shm, wl_shm_pool, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle};

use super::{CompState, SurfaceData};

/// Shm pool backing — the fd the client handed us + mmap of it.
/// Interior mutability: `resize` remaps in place (clients grow
/// pools constantly — reading past the original map length would
/// be garbage or SIGBUS).
pub struct PoolData {
    pub fd: OwnedFd,
    pub map: std::sync::Arc<std::sync::Mutex<PoolMap>>,
}
pub struct PoolMap {
    pub ptr: *mut u8,
    pub len: usize,
}

impl Drop for PoolMap {
    /// Unmap whatever this map currently points at — covers shm
    /// pools (last buffer/pool ref) and dmabuf mmaps alike. The
    /// resize path already rewrote ptr/len under the mutex, so
    /// only the live mapping is released.
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                sys::munmap(self.ptr as *mut std::ffi::c_void, self.len);
            }
        }
    }
}
// The pointer is only ever dereferenced on the single compositor
// thread — the bound exists solely so `U: Send + Sync` holds.
unsafe impl Send for PoolMap {}
unsafe impl Sync for PoolMap {}
unsafe impl Send for PoolData {}
unsafe impl Sync for PoolData {}

/// Per-buffer geometry inside its pool — attached to each buffer.
pub struct BufInfo {
    pub map: std::sync::Arc<std::sync::Mutex<PoolMap>>,
    pub offset: i32,
    pub w: i32,
    pub h: i32,
    pub stride: i32,
    /// dmabuf descriptor when this wl_buffer wraps GPU memory —
    /// `Some` lets the Vulkan backend import the fd as a sampled
    /// image (zero-copy) instead of CPU-reading `map`. The mmap
    /// stays as the CPU/fallback path's view of the same pixels.
    pub dma: Option<DmaRef>,
    /// Pixel format — wl_shm::Format value for shm buffers,
    /// the drm fourcc for dmabuf buffers. Icon snapshots and
    /// any CPU pixel path need it for alpha swizzles.
    pub format: u32,
}

/// One imported dmabuf plane — everything
/// `VkImageDrmFormatModifierExplicitCreateInfoEXT` plus the
/// memory-fd import need to rebuild the image.
pub struct DmaRef {
    /// The client's plane fd — dup'd at import time; the fd
    /// itself (not its number) is the buffer identity.
    pub fd: std::os::fd::OwnedFd,
    /// Plane byte offset inside the dmabuf.
    pub offset: u64,
    /// Plane row pitch in bytes.
    pub stride: u32,
    /// DRM format modifier — LINEAR or INVALID only, everything
    /// else is rejected at `create`/`create_immed` time.
    pub modifier: u64,
    /// fourcc — XRGB8888/ARGB8888; the import view needs it to
    /// pick the alpha swizzle.
    pub format: u32,
}

pub(crate) mod sys {
    use std::ffi::c_void;
    #[link(name = "c")]
    extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
    }
    pub const PROT_READ: i32 = 0x1;
    pub const PROT_WRITE: i32 = 0x2;
    pub const MAP_SHARED: i32 = 0x01;
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_ANONYMOUS: i32 = 0x20;
}

impl Dispatch<wl_shm::WlShm, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        // wl_shm v2 release — the client drops its global binding;
        // nothing server-side holds per-binding state.
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            let ptr = unsafe {
                sys::mmap(
                    std::ptr::null_mut(),
                    size as usize,
                    // PROT_WRITE too — screencopy composites
                    // straight into the client's pool.
                    sys::PROT_READ | sys::PROT_WRITE,
                    sys::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            };
            // A failed mmap (e.g. a write-only fd) gets normalized
            // to a null map — every consumer can bail on a single
            // is_null() check instead of testing for MAP_FAILED.
            // Warn once here, not per-frame.
            if ptr as isize == -1 {
                warn!(size, "shm pool mmap failed — buffers draw black");
            }
            init.init::<wl_shm_pool::WlShmPool, PoolData>(
                id,
                PoolData {
                    fd,
                    map: std::sync::Arc::new(std::sync::Mutex::new(PoolMap {
                        ptr: if ptr as isize == -1 {
                            std::ptr::null_mut()
                        } else {
                            ptr as *mut u8
                        },
                        len: if ptr as isize == -1 { 0 } else { size as usize },
                    })),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, PoolData> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        pool: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &PoolData,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::Resize { size } => {
                let mut m = data.map.lock().unwrap();
                // wl_shm_pool.resize may only grow the pool — a
                // shrink would invalidate every buffer that was
                // validated against the old length. That is a
                // protocol error, not a silent clamp.
                if (size as usize) < m.len {
                    dh.post_error(
                        pool,
                        wl_shm::Error::InvalidFd as u32,
                        "shrinking shm pool resize".to_string(),
                    );
                    return;
                }
                let p = unsafe {
                    sys::mmap(
                        std::ptr::null_mut(),
                        size as usize,
                        sys::PROT_READ | sys::PROT_WRITE,
                        sys::MAP_SHARED,
                        data.fd.as_raw_fd(),
                        0,
                    )
                };
                // Only unmap the old region once the replacement
                // exists — on failure the pool keeps its old
                // mapping and remains fully usable.
                if p as isize == -1 {
                    warn!(size, "shm pool resize mmap failed — keeping old map");
                    return;
                }
                unsafe {
                    sys::munmap(m.ptr as *mut std::ffi::c_void, m.len);
                }
                m.ptr = p as *mut u8;
                m.len = size as usize;
                info!(size, "shm pool resized + remapped");
            }
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                // Every field is client-controlled — validate the
                // whole data range before the buffer may exist:
                // negative offsets, a stride shorter than a row,
                // and any rectangle reaching past the pool's map
                // would turn the raw-pointer render paths into
                // out-of-bounds access. wl_shm has no dedicated
                // "out of range" error; invalid_stride is the
                // protocol-level rejection upstream sends for
                // geometry that cannot describe a valid layout.
                let ok = {
                    let m = data.map.lock().unwrap();
                    let (off, st, w, h) =
                        (offset as i64, stride as i64, width as i64, height as i64);
                    off >= 0
                        && w > 0
                        && h > 0
                        && st >= w * 4
                        && off + (h - 1) * st + w * 4 <= m.len as i64
                };
                if !ok {
                    // The new id still must be born — an uninit'd
                    // object is a wayland-backend panic that kills
                    // the compositor, not just the client. A dead
                    // buffer (null map) exists so the protocol
                    // error lands on the client cleanly.
                    init.init::<wl_buffer::WlBuffer, BufInfo>(
                        id,
                        BufInfo {
                            map: std::sync::Arc::new(std::sync::Mutex::new(PoolMap {
                                ptr: std::ptr::null_mut(),
                                len: 0,
                            })),
                            offset,
                            w: width,
                            h: height,
                            stride,
                            dma: None,
                            format: format.into(),
                        },
                    );
                    dh.post_error(
                        pool,
                        wl_shm::Error::InvalidStride as u32,
                        format!(
                            "invalid buffer geometry off={offset} {width}x{height} stride={stride}"
                        ),
                    );
                    return;
                }
                init.init::<wl_buffer::WlBuffer, BufInfo>(
                    id,
                    BufInfo {
                        map: data.map.clone(),
                        offset,
                        w: width,
                        h: height,
                        stride,
                        dma: None,
                        format: format.into(),
                    },
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufInfo> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &BufInfo,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

/// wl_region contents — a rect list the pointer-constraints
/// protocol reads back when a client confines/locks its pointer.
/// Kept on the region object itself so region_rects() can grab
/// it without a side table.
#[derive(Default)]
pub struct RegionData {
    /// Rectangles (x, y, w, h) in surface-local space — the
    /// union of them forms the region.
    pub rects: std::sync::Mutex<Vec<(i32, i32, i32, i32)>>,
}

impl Dispatch<wl_region::WlRegion, RegionData> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        request: wl_region::Request,
        data: &RegionData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_region::Request::Add {
                x,
                y,
                width,
                height,
            } => {
                // Zero/negative-area rects are spec'd as no-ops.
                if width > 0 && height > 0 {
                    data.rects.lock().unwrap().push((x, y, width, height));
                }
            }
            wl_region::Request::Subtract {
                x,
                y,
                width,
                height,
            } => {
                // Rect subtraction splits each stored rect into
                // up to four remainders around the cut-out.
                let mut out = Vec::new();
                for &(rx, ry, rw, rh) in data.rects.lock().unwrap().iter() {
                    out.extend(rect_subtract(rx, ry, rw, rh, x, y, width, height));
                }
                *data.rects.lock().unwrap() = out;
            }
            wl_region::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Subtract rectangle (bx,by,bw,bh) from (ax,ay,aw,ah) — returns
/// the up-to-four rects covering what remains of A.
fn rect_subtract(
    ax: i32,
    ay: i32,
    aw: i32,
    ah: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
) -> Vec<(i32, i32, i32, i32)> {
    let (ax2, ay2, bx2, by2) = (ax + aw, ay + ah, bx + bw, by + bh);
    // No overlap → A survives whole.
    if bx >= ax2 || bx2 <= ax || by >= ay2 || by2 <= ay {
        return vec![(ax, ay, aw, ah)];
    }
    let mut out = Vec::new();
    // Top and bottom bands span A's full width.
    if ay < by {
        out.push((ax, ay, aw, by - ay));
    }
    if by2 < ay2 {
        out.push((ax, by2, aw, ay2 - by2));
    }
    // Side bands span only the middle vertical strip.
    let (my1, my2) = (ay.max(by), ay2.min(by2));
    if ax < bx {
        out.push((ax, my1, bx - ax, my2 - my1));
    }
    if bx2 < ax2 {
        out.push((bx2, my1, ax2 - bx2, my2 - my1));
    }
    out.into_iter()
        .filter(|&(_, _, w, h)| w > 0 && h > 0)
        .collect()
}

impl Dispatch<wayland_server::protocol::wl_compositor::WlCompositor, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wayland_server::protocol::wl_compositor::WlCompositor,
        request: wayland_server::protocol::wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_compositor::Request as R;
        match request {
            R::CreateSurface { id } => {
                let surf =
                    init.init::<wl_surface::WlSurface, SurfaceData>(id, SurfaceData::default());
                // The XWM resolves WL_SURFACE_ID messages against
                // this registry — every surface must be findable
                // by its client-local protocol id.
                state.all_surfaces.push(surf.clone());
                // A pending X window may have claimed this surface
                // id already — the WL_SURFACE_ID message races the
                // surface create across two different sockets.
                state.bind_pending_xwin(&surf);
                info!("client surface created");
            }
            R::CreateRegion { id } => {
                // Regions must be initialized too — an uninit'd New<>
                // is a protocol error that kills the client.
                init.init::<wl_region::WlRegion, RegionData>(id, RegionData::default());
            }
            _ => {}
        }
    }
}
