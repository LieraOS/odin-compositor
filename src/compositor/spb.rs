// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `wp_single_pixel_buffer_v1` — solid-color 1×1 buffers.
//! Clients use them for cursor surfaces, tinted overlays and
//! (with wp_viewport) full-screen fills without a shm pool.
//! We back each one with a real anonymous mmap holding the
//! pixel, so the whole downstream path — CPU blit, Vulkan
//! upload, damage, release — treats it as an ordinary buffer.

use wayland_protocols::wp::single_pixel_buffer::v1::server::{
    wp_single_pixel_buffer_manager_v1 as spb,
    wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
};
use wayland_server::protocol::{wl_buffer, wl_shm};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle};

use super::shm::{sys, BufInfo, PoolMap};
use super::CompState;

pub use wayland_protocols::wp::single_pixel_buffer::v1::server::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1 as WpSpbManager;

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpSinglePixelBufferManagerV1,
        request: spb::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            spb::Request::CreateU32RgbaBuffer { id, r, g, b, a } => {
                // Anonymous private 4-byte map — the "pool" that
                // never existed. munmap runs in PoolMap::drop.
                let ptr = unsafe {
                    sys::mmap(
                        std::ptr::null_mut(),
                        4,
                        sys::PROT_READ | sys::PROT_WRITE,
                        sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    // The id must be born even on mmap failure —
                    // an uninit'd object panics the wayland
                    // backend and kills the compositor whole.
                    // A zero-size map draws nothing, which is the
                    // honest answer for an allocation that failed.
                    init.init::<wl_buffer::WlBuffer, BufInfo>(
                        id,
                        BufInfo {
                            map: std::sync::Arc::new(std::sync::Mutex::new(PoolMap {
                                ptr: std::ptr::null_mut(),
                                len: 0,
                            })),
                            offset: 0,
                            w: 0,
                            h: 0,
                            stride: 0,
                            dma: None,
                            format: wl_shm::Format::Argb8888 as u32,
                        },
                    );
                    return;
                }
                // wl_shm ARGB8888 memory order is B,G,R,A on
                // little-endian — the blit reads alpha at +3.
                let px = [b as u8, g as u8, r as u8, a as u8];
                unsafe { std::ptr::copy_nonoverlapping(px.as_ptr(), ptr as *mut u8, 4) };
                init.init::<wl_buffer::WlBuffer, BufInfo>(
                    id,
                    BufInfo {
                        map: std::sync::Arc::new(std::sync::Mutex::new(PoolMap {
                            ptr: ptr as *mut u8,
                            len: 4,
                        })),
                        offset: 0,
                        w: 1,
                        h: 1,
                        stride: 4,
                        dma: None,
                        // The blit lays out B,G,R,A bytes —
                        // little-endian ARGB8888 by wl_shm name.
                        format: wl_shm::Format::Argb8888 as u32,
                    },
                );
            }
            _ => {}
        }
    }
}
