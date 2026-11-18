// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame backing store for the nested backend — one memfd,
//! persistently mapped, presented straight to the host
//! compositor (zero copy per frame).

use anyhow::Result;
use std::os::fd::{AsFd, AsRawFd};
use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::QueueHandle;

mod sys {
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
    }
    pub const PROT_RW: i32 = 0x3; // PROT_READ|PROT_WRITE
    pub const MAP_SHARED: i32 = 0x01;
}

/// Backing store for presented frames: one memfd, persistently
/// mapped — each frame writes pixels straight into it (zero copy).
pub(crate) struct FrameBuf {
    pub buf: wl_buffer::WlBuffer,
    pub _fd: std::os::fd::OwnedFd,
    pub ptr: *mut u8,
    pub w: u32,
    pub h: u32,
}
impl FrameBuf {
    /// Animate: sliding gradient driven by the frame counter —
    /// proves presents flow continuously, not one-shot. With a
    /// damage clip only the damaged rects repaint — the retained
    /// buffer keeps every other pixel from the last frame.
    pub fn paint(&mut self, tick: u64, clip: Option<&[(i32, i32, i32, i32)]>) {
        let (w, h) = (self.w as usize, self.h as usize);
        let shift = (tick % 256) as u8;
        let rows: Vec<(i32, i32, i32, i32)> = match clip {
            None => vec![(0, 0, w as i32, h as i32)],
            Some(rs) => rs.to_vec(),
        };
        unsafe {
            let px = self.ptr;
            for (rx, ry, rw, rh) in rows {
                for y in ry.max(0)..(ry + rh).min(h as i32) {
                    for x in rx.max(0)..(rx + rw).min(w as i32) {
                        let (x, y) = (x as usize, y as usize);
                        let i = (y * w + x) * 4;
                        *px.add(i) = ((x * 255 / w) as u8).wrapping_add(shift);
                        *px.add(i + 1) = ((y * 255 / h) as u8).wrapping_sub(shift);
                        *px.add(i + 2) = 0x30u8.wrapping_add(shift / 2);
                        *px.add(i + 3) = 0xff;
                    }
                }
            }
        }
    }
}

/// Create a memfd-backed shm buffer of WxH — the frame target.
pub(crate) fn make_frame(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<super::nested::Nested>,
    w: u32,
    h: u32,
) -> Result<FrameBuf> {
    let size = (w * h * 4) as u64;
    let fd = rustix::fs::memfd_create("odin-nested", rustix::fs::MemfdFlags::empty())?;
    rustix::fs::ftruncate(&fd, size)?;
    // Pixels go straight into the memfd the pool reads — zero copy.
    let ptr = unsafe {
        sys::mmap(
            std::ptr::null_mut(),
            size as usize,
            sys::PROT_RW,
            sys::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    anyhow::ensure!(ptr != libc::MAP_FAILED, "mmap failed");
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buf = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        (w * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(FrameBuf {
        buf,
        _fd: fd,
        ptr: ptr as *mut u8,
        w,
        h,
    })
}
