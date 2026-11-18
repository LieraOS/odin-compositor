// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `wp_presentation` — precise presentation feedback for video
//! players and frame-paced apps. Each `feedback` request hands
//! the surface a one-shot callback; at the next present that
//! includes it we report the CLOCK_MONOTONIC present time, the
//! output refresh interval and a rolling sequence number.
//! Destroyed surfaces get `discarded` instead.

pub use wayland_protocols::wp::presentation_time::server::wp_presentation::WpPresentation;
use wayland_protocols::wp::presentation_time::server::{
    wp_presentation::{self},
    wp_presentation_feedback::{self, Kind, WpPresentationFeedback},
};

use wayland_server::{DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::{now_ms, CompState, SurfaceData};

impl GlobalDispatch<WpPresentation, ()> for CompState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &wayland_server::Client,
        resource: New<WpPresentation>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init::<WpPresentation, ()>(resource, ());
    }
}

impl Dispatch<WpPresentation, ()> for CompState {
    fn request(
        _state: &mut Self,
        _client: &wayland_server::Client,
        _resource: &WpPresentation,
        request: wp_presentation::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        dinit: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_presentation::Request::Feedback { surface, callback } => {
                let fb = dinit.init::<WpPresentationFeedback, ()>(callback, ());
                if let Some(sd) = surface.data::<SurfaceData>() {
                    sd.present_fbs.lock().unwrap().push(fb);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for CompState {
    /// One-shot objects — the protocol defines no client
    /// requests on feedback; `destroy` arrives as Resource
    /// teardown, not a request.
    fn request(
        _state: &mut Self,
        _client: &wayland_server::Client,
        _resource: &WpPresentationFeedback,
        _request: wp_presentation_feedback::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _dinit: &mut DataInit<'_, Self>,
    ) {
    }
}

impl CompState {
    /// Fire `presented` for every feedback whose surface just
    /// went through this output's present. `out_name` selects
    /// the wl_output object and the refresh interval reported.
    pub fn present_feedback(&mut self, out_name: &str) {
        // sync_output must carry THIS client's wl_output — every
        // client binds its own wl_output resource, and posting an
        // object from a different client is a fatal protocol bug.
        let outs = self.outputs.clone();
        let hz = self
            .layout
            .outputs
            .get(out_name)
            .map(|o| o.mode.2)
            .unwrap_or(60)
            .max(1);
        let refresh = 1_000_000_000u32 / hz;
        self.present_seq = self.present_seq.wrapping_add(1);
        let seq = self.present_seq;
        // CLOCK_MONOTONIC — the clock presentation-time is
        // defined against.
        let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let (sec, nsec) = (ts.tv_sec as u64, ts.tv_nsec as u32);
        for surf in self
            .order
            .iter()
            .chain(self.popups.iter().map(|p| &p.surf))
            .chain(self.layers.iter().map(|l| &l.surf))
        {
            let Some(sd) = surf.data::<SurfaceData>() else {
                continue;
            };
            let mut fbs = sd.present_fbs.lock().unwrap();
            for fb in fbs.drain(..) {
                if !fb.is_alive() {
                    continue;
                }
                let fb_client = fb.client().map(|c| c.id());
                if let Some((o, _)) = outs
                    .iter()
                    .find(|(o, n)| n == out_name && o.client().map(|c| c.id()) == fb_client)
                {
                    fb.sync_output(o);
                }
                fb.presented(
                    (sec >> 32) as u32,
                    sec as u32,
                    nsec,
                    refresh,
                    (seq >> 32) as u32,
                    seq as u32,
                    Kind::empty(),
                );
            }
        }
    }
}

impl CompState {
    pub fn frame_done(&mut self, out_name: &str) {
        self.present_feedback(out_name);
        // Deferred buffer release — two presented frames of
        // delay so the last frame still scanning an old dmabuf
        // is off the CRTC before the client may rewrite it.
        for b in self.release_ready.drain(..) {
            if b.is_alive() {
                b.release();
            }
        }
        std::mem::swap(&mut self.release_ready, &mut self.release_next);
        self.release_next.extend(self.pending_release.drain(..));
        // Syncobj release points ride the identical staging —
        // signaled two presented frames after their buffer left.
        self.signal_releases();
        let now = now_ms();
        for surf in self
            .order
            .iter()
            .chain(self.popups.iter().map(|p| &p.surf))
            .chain(self.layers.iter().map(|l| &l.surf))
        {
            if let Some(sd) = surf.data::<SurfaceData>() {
                let mut q = sd.frames.lock().unwrap();
                for cb in q.drain(..) {
                    cb.done(now);
                }
            }
        }
    }
}
