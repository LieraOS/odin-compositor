// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_image_copy_capture_frame_v1 dispatch — attach buffer,
//! capture, reply. The blit core is identical to
//! zwlr_screencopy's: black backdrop, then the live scene
//! composited straight into the client's shm pool.
//!
//! Reply contract: `transform`, `damage`, `presentation_time`,
//! `ready` — or `failed(reason)` for a dead source, a missing
//! buffer or a geometry mismatch. One capture per frame object,
//! enforced by the `used` latch.

use std::sync::atomic::Ordering;

use wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_frame_v1 as ccf,
    ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
};
use wayland_server::protocol::wl_output;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::copy_capture::CapFrameData;
use super::shm::BufInfo;
use super::CompState;

impl Dispatch<ExtImageCopyCaptureFrameV1, CapFrameData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ccf::Request,
        data: &CapFrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ccf::Request::AttachBuffer { buffer } => {
                *data.buf.lock().unwrap() = Some(buffer);
            }
            ccf::Request::Capture => {
                if data.used.swap(true, Ordering::SeqCst) {
                    frame.failed(ccf::FailureReason::Unknown);
                    return;
                }
                // The source resolves its rect at capture time —
                // a resized toplevel captures its CURRENT size;
                // a dead source is the spec's `stopped` failure.
                let Some((rx, ry, w, h)) = state.cap_src_rect(&data.src) else {
                    frame.failed(ccf::FailureReason::Stopped);
                    return;
                };
                let Some(buf) = data.buf.lock().unwrap().clone() else {
                    // capture before attach_buffer — no pixels to
                    // write into; invalid on the client's side.
                    frame.failed(ccf::FailureReason::Unknown);
                    return;
                };
                let Some(info) = buf.data::<BufInfo>() else {
                    frame.failed(ccf::FailureReason::BufferConstraints);
                    return;
                };
                if info.w != w || info.h != h || info.stride < w * 4 {
                    // Buffer doesn't match the advertised
                    // buffer_size — BufferConstraints by spec.
                    frame.failed(ccf::FailureReason::BufferConstraints);
                    return;
                }
                let map = info.map.lock().unwrap();
                let need = info.offset.checked_add(info.stride.saturating_mul(h));
                if info.offset < 0
                    || map.ptr.is_null()
                    || need.is_none_or(|n| n < 0 || n as usize > map.len)
                {
                    frame.failed(ccf::FailureReason::Unknown);
                    return;
                }
                // Same core as zwlr_screencopy: opaque black under
                // the composite — a screenshot expects a backdrop.
                if info.stride == w * 4 {
                    let dst = unsafe {
                        std::slice::from_raw_parts_mut(
                            map.ptr.add(info.offset as usize),
                            (w * 4 * h) as usize,
                        )
                    };
                    for px in dst.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                    }
                    state.composite(dst, w as u32, h as u32, rx, ry, data.paint_cursors, None);
                } else {
                    let mut tmp = vec![0u8; (w * 4 * h) as usize];
                    for px in tmp.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                    }
                    state.composite(
                        &mut tmp,
                        w as u32,
                        h as u32,
                        rx,
                        ry,
                        data.paint_cursors,
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
                frame.transform(wl_output::Transform::Normal);
                // No diff tracking — report the whole frame as
                // damaged, the honest answer.
                frame.damage(0, 0, w, h);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                frame.presentation_time(
                    (now.as_secs() >> 32) as u32,
                    now.as_secs() as u32,
                    now.subsec_nanos(),
                );
                frame.ready();
            }
            _ => {}
        }
    }
}
