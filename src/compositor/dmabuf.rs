// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `zwp_linux_dmabuf_v1` — GPU-sharable buffers for clients
//! that export them (EGL/Vulkan apps, video players, XWayland
//! glamor). Version 4: tranche feedback announces the GPU's
//! real device (main_device) and format/modifier table, so
//! clients pick the right render node instead of guessing —
//! without it XWayland can't find a GBM device at all.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

pub use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::{self},
};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::{DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::shm::PoolMap;
use super::{BufInfo, CompState};

/// fourcc 'XR24' / 'AR24' — the formats our blitter consumes.
const FMT_XRGB8888: u32 = 0x3432_5258;
const FMT_ARGB8888: u32 = 0x3432_5241;
/// DRM_FORMAT_MOD_LINEAR — flat memory, mappable by CPU.
const MOD_LINEAR: u64 = 0;
/// DRM_FORMAT_MOD_INVALID — "unspecified", CPU-linear in
/// practice for shm-exported dmabufs; accept it as linear.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Modifier set the Vulkan backend measured on the physical
/// device at init (SAMPLED B8G8R8A8) — published here so the
/// protocol global advertises real GPU tiling instead of just
/// LINEAR. Stays empty on CPU-renderer sessions.
static ADVERTISED_MODS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Called once by the renderer at device init — the list is
/// what the driver actually samples, never a hardcoded guess.
pub(crate) fn set_advertised_modifiers(mods: Vec<u64>) {
    *ADVERTISED_MODS.lock().unwrap() = mods;
}

/// The accepted modifier set: GPU-enumerated modifiers plus
/// LINEAR/INVALID for the CPU-mappable fallback. Clients must
/// pick from what we advertised.
fn accepted_modifiers() -> Vec<u64> {
    let mut m = ADVERTISED_MODS.lock().unwrap().clone();
    m.push(MOD_LINEAR);
    m.push(MOD_INVALID);
    m
}

/// One plane announcement the client made via `params.add`.
struct Plane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

/// Accumulated plane set — lives in the params object's udata.
type Params = Mutex<Vec<Plane>>;

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
    pub const PROT_READ: i32 = 0x1;
    pub const MAP_SHARED: i32 = 0x01;
}

/// Validate the accumulated planes + geometry and mmap the
/// buffer into a PoolMap the normal blit path can read.
/// Returns None on any malformed input — caller emits `failed`.
fn validate_and_map(planes: &[Plane], w: i32, h: i32, format: u32) -> Option<(BufInfo, usize)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    if !matches!(format, FMT_XRGB8888 | FMT_ARGB8888) {
        return None;
    }
    // Single-plane formats only — multi-plane needs a GPU path.
    if planes.len() != 1 {
        return None;
    }
    let p = &planes[0];
    // Reject modifiers outside the advertised set — a client
    // that proposes one gets params.failed and retries with
    // another entry from the list.
    if !accepted_modifiers().contains(&p.modifier) {
        return None;
    }
    if (p.stride as i32) < w * 4 {
        return None;
    }
    let len = (p.stride as usize) * (h as usize);
    // The mmap is only the CPU-fallback view — a GPU-side import
    // consumes the fd directly. Tiled modifiers can't be CPU-
    // mapped usefully at all, so only LINEAR-family buffers get
    // the map attempt.
    let ptr = if matches!(p.modifier, MOD_LINEAR | MOD_INVALID) {
        let ptr = unsafe {
            sys::mmap(
                std::ptr::null_mut(),
                len,
                sys::PROT_READ,
                sys::MAP_SHARED,
                p.fd.as_raw_fd(),
                p.offset as i64,
            )
        };
        if ptr.is_null() || ptr as isize == -1 {
            std::ptr::null_mut()
        } else {
            ptr as *mut u8
        }
    } else {
        std::ptr::null_mut()
    };
    let map = PoolMap { ptr, len };
    let dma = match p.fd.try_clone() {
        Ok(fd) => super::shm::DmaRef {
            fd,
            offset: p.offset as u64,
            stride: p.stride,
            // INVALID normalizes to LINEAR — the Vulkan import
            // needs a concrete modifier, not "unspecified".
            modifier: if p.modifier == MOD_INVALID {
                MOD_LINEAR
            } else {
                p.modifier
            },
            format,
        },
        Err(_) => return None,
    };
    Some((
        BufInfo {
            map: Arc::new(Mutex::new(map)),
            offset: 0,
            w,
            h,
            stride: p.stride as i32,
            dma: Some(dma),
            format,
        },
        len,
    ))
}

impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for CompState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &wayland_server::Client,
        resource: New<ZwpLinuxDmabufV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let dm = data_init.init::<ZwpLinuxDmabufV1, ()>(resource, ());
        // What we can consume: two XRGB-class formats, with the
        // full GPU-measured modifier set — a real driver list so
        // clients can hand us tiled dmabufs (XWayland DRI3 needs
        // this for glamor; LINEAR alone forces software fallback).
        dm.format(FMT_XRGB8888);
        dm.format(FMT_ARGB8888);
        if dm.version() >= 3 {
            for m in accepted_modifiers() {
                for f in [FMT_XRGB8888, FMT_ARGB8888] {
                    dm.modifier(f, (m >> 32) as u32, m as u32);
                }
            }
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _client: &wayland_server::Client,
        _resource: &ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        dinit: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                dinit.init::<ZwpLinuxBufferParamsV1, Params>(params_id, Mutex::new(Vec::new()));
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id } => {
                let fb = dinit.init::<ZwpLinuxDmabufFeedbackV1, ()>(id, ());
                state.send_dmabuf_feedback(&fb);
            }
            zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => {
                // One GPU, one tranche — per-surface feedback is
                // identical to the default on single-device rigs.
                let fb = dinit.init::<ZwpLinuxDmabufFeedbackV1, ()>(id, ());
                state.send_dmabuf_feedback(&fb);
            }
            // `destroy` (v3+) is a destructor — the server drops
            // the resource on its own after dispatch.
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for CompState {
    fn request(
        _state: &mut Self,
        _client: &wayland_server::Client,
        _resource: &ZwpLinuxDmabufFeedbackV1,
        _request: zwp_linux_dmabuf_feedback_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _dinit: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` exists; destructors are auto-handled.
    }
}

/// The format/modifier table as the protocol wants it —
/// flat {u32 format, u32 pad, u64 modifier} records written
/// to a fresh memfd the client mmaps read-only.
fn format_table_fd() -> Option<OwnedFd> {
    use std::os::unix::io::FromRawFd;
    let fd = unsafe { libc::memfd_create(c"dmabuf-format-table".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    for f in [FMT_XRGB8888, FMT_ARGB8888] {
        for m in accepted_modifiers() {
            let mut rec = [0u8; 16];
            rec[0..4].copy_from_slice(&f.to_ne_bytes());
            rec[8..16].copy_from_slice(&m.to_ne_bytes());
            if file.write_all(&rec).is_err() {
                return None;
            }
        }
    }
    Some(file.into())
}

impl CompState {
    /// Emit the full v4 feedback sequence: format table, main
    /// device, one tranche covering every index, done. The
    /// device dev_t lets clients resolve /dev/dri/* themselves —
    /// XWayland's glamor init depends on exactly this.
    fn send_dmabuf_feedback(&self, fb: &ZwpLinuxDmabufFeedbackV1) {
        let Some(fd) = format_table_fd() else {
            fb.done();
            return;
        };
        let size = (accepted_modifiers().len() * 2 * 16) as u32;
        fb.format_table(fd.as_fd(), size);
        fb.main_device(self.drm_devt.to_ne_bytes().to_vec());
        fb.tranche_target_device(self.drm_devt.to_ne_bytes().to_vec());
        fb.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout);
        // tranche_formats carries a byte array of u16 indices
        // — one u16 per (format, modifier) record in the table.
        let count = (accepted_modifiers().len() * 2) as u16;
        let idx: Vec<u8> = (0..count).flat_map(u16::to_ne_bytes).collect();
        fb.tranche_formats(idx);
        fb.tranche_done();
        fb.done();
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, Params> for CompState {
    fn request(
        state: &mut Self,
        _client: &wayland_server::Client,
        params: &ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &Params,
        _dhandle: &DisplayHandle,
        dinit: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx: _,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                data.lock().unwrap().push(Plane {
                    fd,
                    offset,
                    stride,
                    modifier,
                });
            }
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                flags: _,
            } => {
                let planes = std::mem::take(&mut *data.lock().unwrap());
                match validate_and_map(&planes, width, height, format) {
                    Some((info, _)) => {
                        if let Some(client) = params.client() {
                            if let Ok(buf) = client
                                .create_resource::<WlBuffer, BufInfo, CompState>(&state.dh, 1, info)
                            {
                                params.created(&buf);
                                return;
                            }
                        }
                        params.failed();
                    }
                    None => params.failed(),
                }
            }
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags: _,
            } => {
                let planes = std::mem::take(&mut *data.lock().unwrap());
                match validate_and_map(&planes, width, height, format) {
                    Some((info, _)) => {
                        dinit.init::<WlBuffer, BufInfo>(buffer_id, info);
                    }
                    None => {
                        // Protocol: a rejected create_immed still
                        // owes the client a real wl_buffer — the
                        // `failed` event only says the pixels are
                        // unusable, and the client destroys the
                        // dead buffer itself. Leaving the id
                        // uninit'd is a wayland-backend panic
                        // ("no object data") that aborts the
                        // whole compositor — the Electron crash.
                        dinit.init::<WlBuffer, BufInfo>(
                            buffer_id,
                            BufInfo {
                                map: std::sync::Arc::new(std::sync::Mutex::new(
                                    super::shm::PoolMap {
                                        ptr: std::ptr::null_mut(),
                                        len: 0,
                                    },
                                )),
                                offset: 0,
                                w: width,
                                h: height,
                                stride: 0,
                                dma: None,
                                format,
                            },
                        );
                        params.failed();
                    }
                }
            }
            _ => {}
        }
    }
}
