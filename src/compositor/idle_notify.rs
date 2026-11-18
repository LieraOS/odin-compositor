// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_idle_notifier_v1 — client-observable idle notifications.
//!
//! Session components (screenlock, power management, night-mode
//! daemons) register a timeout against a seat and get `idled`
//! when no input has been seen for that long, `resumed` on the
//! first event afterwards. Activity is stamped into
//! `CompState::last_input` by the pointer/keyboard entry points;
//! a per-frame `idle_tick` evaluates every live notification so
//! idleness reports even when nothing else wakes the loop.

use std::time::{Duration, Instant};

use tracing::info;
use wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1 as ein, ext_idle_notification_v1::ExtIdleNotificationV1,
    ext_idle_notifier_v1 as enr, ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_server::protocol::wl_seat;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

pub use wayland_protocols::ext::idle_notify::v1::server::ext_idle_notifier_v1::ExtIdleNotifierV1 as ExtIdleNotifier;

/// One live notification — the object plus its armed state.
pub struct IdleNotif {
    pub obj: ExtIdleNotificationV1,
    /// Which seat's activity this watches — recorded for the day
    /// a second seat exists; today every watcher sees the one seat.
    #[allow(dead_code)]
    pub seat: wayland_server::backend::ObjectId,
    pub timeout: Duration,
    /// Edge-detection latch — events fire on transitions only.
    pub idled: bool,
}

impl CompState {
    /// Per-frame scan: fire `idled`/`resumed` transitions and
    /// drop notifications whose object died. Runs on the frame
    /// tick so timeouts still report while the scene is static.
    pub fn idle_tick(&mut self) {
        let now = Instant::now();
        let idle_for = now.duration_since(self.last_input);
        for n in &mut self.idle_notifs {
            if idle_for >= n.timeout {
                if !n.idled {
                    n.idled = true;
                    n.obj.idled();
                }
            } else if n.idled {
                n.idled = false;
                n.obj.resumed();
            }
        }
        self.idle_notifs.retain(|n| n.obj.is_alive());
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtIdleNotifierV1,
        request: enr::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            enr::Request::GetIdleNotification { id, seat, timeout } => {
                state.arm_idle_notification(id, seat, timeout, init);
            }
            // v2 request — identical semantics; Odin only tracks
            // hardware input anyway, so the "input device only"
            // variant maps onto the same watcher.
            enr::Request::GetInputIdleNotification { id, seat, timeout } => {
                state.arm_idle_notification(id, seat, timeout, init);
            }
            _ => {}
        }
    }
}

impl CompState {
    fn arm_idle_notification(
        &mut self,
        id: wayland_server::New<ExtIdleNotificationV1>,
        seat: wl_seat::WlSeat,
        timeout: u32,
        init: &mut DataInit<'_, Self>,
    ) {
        let obj = init.init::<ExtIdleNotificationV1, ()>(id, ());
        info!(timeout, "idle notification armed");
        self.idle_notifs.push(IdleNotif {
            obj,
            seat: seat.id(),
            timeout: Duration::from_millis(timeout as u64),
            idled: false,
        });
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtIdleNotificationV1,
        request: ein::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // The only request is destroy — the dead object drops out
        // of idle_notifs on the next idle_tick's retain pass.
        let _ = request;
    }
}
