// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_screencopy_v1 — screenshots and screen sharing.
//!
//! The manager hands out one frame object per capture request.
//! We advertise shm geometry via `buffer` (+ `buffer_done` on
//! v3 binds), the client hands back a wl_shm buffer via `copy`,
//! and we composite the captured rect straight into its pool —
//! no intermediate allocation on the hot path, no GPU
//! dependency. (Zero-copy dmabuf capture is roadmap material.)
//!
//! The capture is the CURRENT scene state, taken at copy time —
//! grim-style one-shot screenshots don't need vblank sync.

use std::sync::atomic::{AtomicBool, Ordering};

use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1 as scf, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1 as scm, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_server::protocol::wl_shm;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{BufInfo, CompState};

// Re-export — mod.rs names the global through this path.
pub use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 as ZwlrScreencopyManager;

/// Per-frame state on the frame object's user data.
struct FrameData {
    /// Capture rect in GLOBAL space — the output origin and any
    /// requested region offset are already folded in.
    rect: (i32, i32, i32, i32),
    /// Client asked for the cursor inside the capture.
    overlay: bool,
    /// Spec: exactly one copy per frame object.
    used: AtomicBool,
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrScreencopyManagerV1,
        request: scm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            scm::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let (x, y, w, h) = output_rect(state, &output);
                let frame = init.init::<ZwlrScreencopyFrameV1, FrameData>(
                    frame,
                    FrameData {
                        rect: (x, y, w as i32, h as i32),
                        overlay: overlay_cursor != 0,
                        used: AtomicBool::new(false),
                    },
                );
                send_buffer_offer(&frame, w, h);
            }
            scm::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                x,
                y,
                width,
                height,
                output,
            } => {
                // Region coords are output-local — translate into
                // global space so the blit viewport lands right.
                let (ox, oy, ..) = output_rect(state, &output);
                let frame = init.init::<ZwlrScreencopyFrameV1, FrameData>(
                    frame,
                    FrameData {
                        rect: (ox + x, oy + y, width, height),
                        overlay: overlay_cursor != 0,
                        used: AtomicBool::new(false),
                    },
                );
                send_buffer_offer(&frame, width as u32, height as u32);
            }
            scm::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Resolve the wl_output a client passed to its global-space
/// rect — the bound resource's userdata carries the connector
/// name. A stale/dead output (hot-unplugged between bind and
/// capture) falls back to the primary rect rather than failing.
fn output_rect(
    state: &CompState,
    output: &wayland_server::protocol::wl_output::WlOutput,
) -> (i32, i32, u32, u32) {
    output
        .data::<String>()
        .and_then(|name| state.layout.outputs.get(name.as_str()))
        .filter(|o| o.enabled)
        .map(|o| (o.x, o.y, o.mode.0, o.mode.1))
        .unwrap_or_else(|| state.layout.primary_rect())
}

/// Offer the shm geometry the client must allocate — XRGB8888
/// at output size, tight stride. On v3 the offer closes with
/// `buffer_done`; we don't send `linux_dmabuf` because dmabuf
/// capture isn't implemented (clients fall back to shm).
fn send_buffer_offer(frame: &ZwlrScreencopyFrameV1, w: u32, h: u32) {
    frame.buffer(wl_shm::Format::Xrgb8888, w, h, w * 4);
    if frame.version() >= 3 {
        frame.buffer_done();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: scf::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            scf::Request::Copy { buffer } | scf::Request::CopyWithDamage { buffer } => {
                if data.used.swap(true, Ordering::SeqCst) {
                    frame.failed();
                    return;
                }
                let Some(info) = buffer.data::<BufInfo>() else {
                    frame.failed();
                    return;
                };
                let (_, _, w, h) = data.rect;
                if info.w != w || info.h != h || info.stride < w * 4 {
                    // Client gave us a buffer that doesn't match
                    // the advertised geometry — protocol-level
                    // failure, not a compositor bug.
                    frame.failed();
                    return;
                }
                let map = info.map.lock().unwrap();
                // offset/stride are i32 at the type level — a
                // negative offset would wrap `ptr.add(offset as
                // usize)` into an out-of-mapping write, and a
                // stride*h overflow could wrap `need` back into
                // range. Reject both before any pointer math.
                let need = info.offset.checked_add(info.stride.saturating_mul(h));
                if info.offset < 0
                    || map.ptr.is_null()
                    || need.is_none_or(|n| n < 0 || n as usize > map.len)
                {
                    frame.failed();
                    return;
                }
                // composite() draws surfaces only — the backdrop
                // a screenshot expects is our job: opaque black
                // under everything.
                if info.stride == w * 4 {
                    // Tight stride — composite straight into the
                    // client's pool, zero intermediate copies.
                    let dst = unsafe {
                        std::slice::from_raw_parts_mut(
                            map.ptr.add(info.offset as usize),
                            (w * 4 * h) as usize,
                        )
                    };
                    for px in dst.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                    }
                    state.composite(
                        dst,
                        w as u32,
                        h as u32,
                        data.rect.0,
                        data.rect.1,
                        data.overlay,
                        None,
                    );
                } else {
                    // Padded stride — composite into a scratch
                    // buffer, then copy row by row.
                    let mut tmp = vec![0u8; (w * 4 * h) as usize];
                    for px in tmp.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                    }
                    state.composite(
                        &mut tmp,
                        w as u32,
                        h as u32,
                        data.rect.0,
                        data.rect.1,
                        data.overlay,
                        None,
                    );
                    let dst = unsafe { map.ptr.add(info.offset as usize) };
                    for row in 0..h as usize {
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                tmp.as_ptr().add(row * (w * 4) as usize),
                                dst.add(row * info.stride as usize),
                                (w * 4) as usize,
                            );
                        }
                    }
                }
                // Contents are rows top-to-bottom — no y_invert.
                frame.flags(scf::Flags::empty());
                if frame.version() >= 2 {
                    // No per-surface damage tracking yet — report
                    // the whole capture region as changed.
                    frame.damage(0, 0, w as u32, h as u32);
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                frame.ready(
                    (now.as_secs() >> 32) as u32,
                    now.as_secs() as u32,
                    now.subsec_nanos(),
                );
            }
            scf::Request::Destroy => {}
            _ => {}
        }
    }
}
