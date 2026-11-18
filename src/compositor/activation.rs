// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! xdg_activation_v1 — launch-focus tokens.
//!
//! Launchers hand a token to the app they spawn; the app calls
//! `activate(token, surface)` to pull focus on startup instead of
//! racing focus_on_map. Odin's policy: a committed token is valid
//! — the metadata (app_id, seat, serial) is recorded for logging
//! and future stricter policies, not to reject legitimate focus.
//! Tokens are single-use and expire after 60 s.

use std::time::Instant;

use tracing::{info, warn};
use wayland_protocols::xdg::activation::v1::server::{
    xdg_activation_token_v1 as xat, xdg_activation_token_v1::XdgActivationTokenV1,
    xdg_activation_v1 as xav, xdg_activation_v1::XdgActivationV1,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols::xdg::activation::v1::server::xdg_activation_v1::XdgActivationV1 as XdgActivation;

/// Metadata attached to a committed token — the compositor's
/// activation policy can weigh these when it gets stricter.
#[derive(Default)]
pub struct ActToken {
    pub app_id: Option<String>,
    pub serial: Option<u32>,
    pub surface: Option<String>,
    pub created: Option<Instant>,
}

impl CompState {
    /// Generate an unpredictable token id — 16 bytes from the
    /// kernel RNG, hex-encoded (matches other compositors' scheme).
    fn new_token_id() -> String {
        use std::io::Read;
        let mut buf = [0u8; 16];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut buf);
        }
        buf.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Drop stale tokens — called opportunistically on activate.
    fn prune_tokens(&mut self) {
        let now = Instant::now();
        self.act_tokens.retain(|_, t| {
            t.created
                .map(|c| now.duration_since(c).as_secs() < 60)
                .unwrap_or(false)
        });
    }
}

impl Dispatch<XdgActivationV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgActivationV1,
        request: xav::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xav::Request::GetActivationToken { id } => {
                init.init::<XdgActivationTokenV1, std::sync::Mutex<ActToken>>(
                    id,
                    std::sync::Mutex::new(ActToken::default()),
                );
            }
            xav::Request::Activate { token, surface } => {
                state.activate_with_token(token, surface);
            }
            _ => {}
        }
    }
}

impl CompState {
    fn activate_with_token(&mut self, token: String, surface: wl_surface::WlSurface) {
        self.prune_tokens();
        let Some(meta) = self.act_tokens.remove(&token) else {
            warn!("xdg_activation: unknown or expired token — ignored");
            return;
        };
        let Some(sd) = surface.data::<super::SurfaceData>() else {
            return;
        };
        if !Self::has_window_role(sd) {
            warn!("xdg_activation: target is not a toplevel — ignored");
            return;
        }
        info!(
            app_id = meta.app_id.as_deref().unwrap_or("?"),
            "xdg_activation: focus via token"
        );
        // Un-minimize so activation actually shows the window,
        // then raise + keyboard focus — same path a taskbar click
        // takes through foreign-toplevel activate.
        sd.minimized
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.raise(&surface);
        self.set_kbd_focus(Some(surface));
        self.damage_everything();
    }
}

impl Dispatch<XdgActivationTokenV1, std::sync::Mutex<ActToken>> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        res: &XdgActivationTokenV1,
        request: xat::Request,
        data: &std::sync::Mutex<ActToken>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            xat::Request::SetSerial { serial, .. } => {
                data.lock().unwrap().serial = Some(serial);
            }
            xat::Request::SetAppId { app_id } => {
                data.lock().unwrap().app_id = Some(app_id);
            }
            xat::Request::SetSurface { surface } => {
                data.lock().unwrap().surface = Some(format!("{}", surface.id()));
            }
            xat::Request::Commit => {
                let id = CompState::new_token_id();
                let mut meta = data.lock().unwrap().clone2();
                meta.created = Some(Instant::now());
                res.done(id.clone());
                state.act_tokens.insert(id, meta);
            }
            _ => {}
        }
    }
}

impl ActToken {
    /// Mutex-held copy without deriving Clone on the struct
    /// (Instant isn't Copy and cloning under the lock is all
    /// the commit path needs).
    fn clone2(&self) -> Self {
        ActToken {
            app_id: self.app_id.clone(),
            serial: self.serial,
            surface: self.surface.clone(),
            created: self.created,
        }
    }
}
