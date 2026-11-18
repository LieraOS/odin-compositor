// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_image_copy_capture_v1 + ext_image_capture_source_v1 —
//! the stabilized successor of zwlr_screencopy for screenshots
//! and screen sharing (newer portals/OBS prefer it).
//!
//! Objects: two source managers hand out sources for outputs
//! and foreign-toplevel handles; the copy manager turns a
//! source into a session (advertises buffer geometry via
//! buffer_size/shm_format/done), sessions mint frames. Frame
//! dispatch + the shm blit live in `copy_frame.rs`.
//!
//! `stopped` semantics are honest: sessions bound to a dying
//! source (output unplugged, toplevel closed) get `stopped`,
//! never silent dead data.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use wayland_protocols::ext::image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as ftsm,
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1 as csrc, ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1 as osm,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1 as ccsr,
    ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1,
    ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
    ext_image_copy_capture_manager_v1 as ccm,
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    ext_image_copy_capture_session_v1 as ccs,
    ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
};
use wayland_server::protocol::{wl_buffer, wl_shm, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::shm::BufInfo;
use super::{CompState, SurfaceData};

pub use wayland_protocols::ext::image_capture_source::v1::server::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1 as ExtForeignToplevelSourceManager;
pub use wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1 as ExtOutputSourceManager;
pub use wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 as ExtCopyCaptureManager;

/// What a source points at — resolved once at creation so the
/// session survives manager/client churn around it.
#[derive(Clone)]
pub enum CapSrc {
    /// A monitor by connector name — capture = its output rect.
    Output(String),
    /// A foreign toplevel — capture = the window's rect.
    Toplevel(wl_surface::WlSurface),
}

/// Session state: the resolved source, the paint_cursors
/// option bit, and the one-frame-at-a-time latch the spec's
/// duplicate_frame error hangs on.
pub struct SessData {
    pub src: CapSrc,
    pub paint_cursors: bool,
    pub frame_live: AtomicBool,
}

/// Frame state: buffers attach first, capture fires once; the
/// source + cursor flag ride along so the frame never has to
/// hunt its session down.
pub struct CapFrameData {
    pub src: CapSrc,
    pub paint_cursors: bool,
    pub buf: Mutex<Option<wl_buffer::WlBuffer>>,
    pub used: AtomicBool,
}

impl CompState {
    /// The capture rect a source covers right now — None means
    /// the source died (session must stop, not fake a frame).
    pub(crate) fn cap_src_rect(&self, src: &CapSrc) -> Option<(i32, i32, i32, i32)> {
        match src {
            CapSrc::Output(name) => self
                .layout
                .outputs
                .get(name)
                .filter(|o| o.enabled)
                .map(|o| (o.x, o.y, o.mode.0 as i32, o.mode.1 as i32)),
            CapSrc::Toplevel(surf) => {
                let sd = surf.data::<SurfaceData>()?;
                let (px, py) = *sd.pos.lock().unwrap();
                let (bw, bh) = sd
                    .shown_buf
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|b| b.data::<BufInfo>())
                    .map(|bi| (bi.w, bi.h))
                    .unwrap_or((0, 0));
                if bw <= 0 || bh <= 0 {
                    return None;
                }
                Some((px, py, bw, bh))
            }
        }
    }

    /// Session hello burst — geometry, shm formats, done.
    /// dmabuf events stay absent: capture is the shm blit path.
    fn cap_session_hello(sess: &ExtImageCopyCaptureSessionV1, w: i32, h: i32) {
        sess.buffer_size(w.max(0) as u32, h.max(0) as u32);
        sess.shm_format(wl_shm::Format::Xrgb8888);
        sess.done();
    }

    /// A source died — every live session on it gets `stopped`
    /// and is dropped from the registry. Called from output
    /// removal and toplevel close paths.
    pub(crate) fn cap_src_died(&mut self, dead: &CapSrc) {
        self.cap_sessions.retain(|(sess, src)| {
            let same = match (src, dead) {
                (CapSrc::Output(a), CapSrc::Output(b)) => a == b,
                (CapSrc::Toplevel(a), CapSrc::Toplevel(b)) => a.id() == b.id(),
                _ => false,
            };
            if same && sess.is_alive() {
                sess.stopped();
            }
            !same
        });
    }

    /// Build the session object for a resolved source — shared
    /// by the option-valid and the dead-source paths.
    fn cap_session_init(
        &mut self,
        init: &mut DataInit<'_, Self>,
        id: wayland_server::New<ExtImageCopyCaptureSessionV1>,
        src: CapSrc,
        paint_cursors: bool,
    ) {
        let rect = self.cap_src_rect(&src);
        let sess = init.init::<ExtImageCopyCaptureSessionV1, SessData>(
            id,
            SessData {
                src: src.clone(),
                paint_cursors,
                frame_live: AtomicBool::new(false),
            },
        );
        match rect {
            Some((_, _, w, h)) => {
                self.cap_sessions.push((sess.clone(), src));
                Self::cap_session_hello(&sess, w, h);
            }
            // Dead source → object exists but is already stopped.
            None => sess.stopped(),
        }
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        request: osm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let osm::Request::CreateSource { source, output } = request {
            // The connector name rides on the wl_output resource —
            // bound in globals.rs with exactly that udata.
            let name = output.data::<String>().cloned().unwrap_or_default();
            init.init::<ExtImageCaptureSourceV1, CapSrc>(source, CapSrc::Output(name));
        }
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ftsm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ftsm::Request::CreateSource {
            source,
            toplevel_handle,
        } = request
        {
            // ext toplevel handles carry the tracked surface —
            // set in ext_ftl's handle creation.
            if let Some(surf) = toplevel_handle.data::<wl_surface::WlSurface>() {
                init.init::<ExtImageCaptureSourceV1, CapSrc>(
                    source,
                    CapSrc::Toplevel(surf.clone()),
                );
            }
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, CapSrc> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: csrc::Request,
        _: &CapSrc,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` exists — auto-destructor.
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtImageCopyCaptureManagerV1,
        request: ccm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ccm::Request::CreateSession {
                session,
                source,
                options,
            } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                let Some(src) = source.data::<CapSrc>().cloned() else {
                    // Dead source — the session object must still
                    // exist or the backend panics on the unborn
                    // id and kills the whole compositor, not
                    // just this client. `stopped` is the honest
                    // wire answer for a source with no capture.
                    init.init::<ExtImageCopyCaptureSessionV1, SessData>(
                        session,
                        SessData {
                            src: CapSrc::Output(String::new()),
                            paint_cursors: false,
                            frame_live: AtomicBool::new(false),
                        },
                    )
                    .stopped();
                    return;
                };
                // options bit 0 = paint_cursors; unknown bits are
                // the spec's invalid_option error — answered with
                // an immediately stopped session object so the
                // client learns on the wire, not via silence.
                let bits: u32 = options.into();
                if bits & !1 != 0 {
                    let sess = init.init::<ExtImageCopyCaptureSessionV1, SessData>(
                        session,
                        SessData {
                            src,
                            paint_cursors: false,
                            frame_live: AtomicBool::new(false),
                        },
                    );
                    sess.stopped();
                    return;
                }
                state.cap_session_init(init, session, src, bits & 1 != 0);
            }
            ccm::Request::CreatePointerCursorSession { session, .. } => {
                // Cursor capture needs the pointer-tracked session
                // variant — not wired; the object must still
                // exist or the client dies on a dangling id.
                init.init::<ExtImageCopyCaptureCursorSessionV1, ()>(session, ());
            }
            _ => {}
        }
    }
}

/// Cursor-session requests. Pointer tracking for cursor-only
/// frames isn't wired, so `get_capture_session` answers with a
/// session object that immediately goes `stopped` — the honest
/// state instead of frames full of nothing.
impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtImageCopyCaptureCursorSessionV1,
        request: ccsr::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ccsr::Request::GetCaptureSession { session } = request {
            // A cursor session on a source we cannot track is a
            // stopped session — the object exists so the client
            // learns the state on the wire.
            state.cap_session_init(init, session, CapSrc::Output(String::new()), false);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, SessData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        sess: &ExtImageCopyCaptureSessionV1,
        request: ccs::Request,
        data: &SessData,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ccs::Request::CreateFrame { frame } => {
                // The id must be born on every path — an uninit'd
                // object panics the wayland backend and kills the
                // whole compositor, not just the client.
                init.init::<ExtImageCopyCaptureFrameV1, CapFrameData>(
                    frame,
                    CapFrameData {
                        src: data.src.clone(),
                        paint_cursors: data.paint_cursors,
                        buf: Mutex::new(None),
                        used: AtomicBool::new(false),
                    },
                );
                // Spec: at most one live frame per session — the
                // second create_frame is a protocol error.
                if data.frame_live.swap(true, Ordering::SeqCst) {
                    dh.post_error(
                        sess,
                        ccs::Error::DuplicateFrame as u32,
                        "a frame object already exists".to_string(),
                    );
                    return;
                }
            }
            ccs::Request::Destroy => {
                // Registry cleanup — lazily swept with src-died.
                state.cap_sessions.retain(|(s, _)| s.id() != sess.id());
            }
            _ => {}
        }
    }
}
