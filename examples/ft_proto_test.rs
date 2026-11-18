// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Foreign-toplevel protocol test client — binds the manager,
//! prints every handle event (title/app_id/state/closed), then
//! drives the window-management requests end-to-end: minimize
//! the first toplevel, unminimize + activate it, and finally
//! close it, verifying each state round-trip.
//!
//! Run inside a nested odin: WAYLAND_DISPLAY=odin-0
//! Prints TOPLEVEL/TITLE/APPID/STATE/MINIMIZED/CLOSED/RESULT
//! lines for grep-checking.

use wayland_client::protocol::{wl_output, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

// Handle `state` array element ids — mirrors the server-side
// enum values (maximized=0, minimized=1, activated=2, fs=3).
const ST_MINIMIZED: u32 = 1;
const ST_ACTIVATED: u32 = 2;

struct App {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    handle: Option<ZwlrForeignToplevelHandleV1>,
    seat: Option<wl_seat::WlSeat>,
    /// Latest decoded state bits for the tracked handle.
    states: Vec<u32>,
    title: String,
    app_id: String,
    /// Test phase machine: watch → minimize → restore → close.
    phase: u8,
    saw_minimized: bool,
    saw_activated: bool,
    saw_closed: bool,
    done: bool,
}

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        reg: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event as E;
        if let E::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "zwlr_foreign_toplevel_manager_v1" => {
                    app.manager = Some(reg.bind(name, 3, qh, ()));
                    println!("BOUND foreign-toplevel-manager");
                }
                // activate() needs a real seat object even
                // though the compositor ignores its identity.
                "wl_seat" => {
                    app.seat = Some(reg.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
    /// The `toplevel` event carries a new_id — wayland-client
    /// needs this hook to know which proxy type (and udata) the
    /// created child object gets.
    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ZwlrForeignToplevelHandleV1, ()>(()),
            _ => unreachable!(),
        }
    }

    fn event(
        app: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_manager_v1::Event as E;
        match event {
            E::Toplevel { toplevel } => {
                println!("TOPLEVEL id={:?}", toplevel.id());
                // Track the LAST handle — the selftest spawns a
                // sacrificial third terminal for this test so the
                // close request doesn't shrink the dump scene.
                app.handle = Some(toplevel);
            }
            E::Finished => println!("MANAGER-FINISHED"),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event as E;
        match event {
            E::Title { title } => {
                app.title = title.clone();
                println!("TITLE {title}");
            }
            E::AppId { app_id } => {
                app.app_id = app_id.clone();
                println!("APPID {app_id}");
            }
            E::State { state } => {
                // Byte array of u32 enum ids — decode little-endian.
                app.states = state
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
            }
            E::Done => {
                println!("STATE {:?}", app.states);
                if app.states.contains(&ST_MINIMIZED) {
                    app.saw_minimized = true;
                    println!("MINIMIZED-SEEN");
                }
                if app.states.contains(&ST_ACTIVATED) {
                    app.saw_activated = true;
                    println!("ACTIVATED-SEEN");
                }
            }
            E::Closed => {
                println!("CLOSED");
                app.saw_closed = true;
                app.done = true;
            }
            _ => {}
        }
        let _ = handle;
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect");
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App {
        manager: None,
        handle: None,
        seat: None,
        states: Vec::new(),
        title: String::new(),
        app_id: String::new(),
        phase: 0,
        saw_minimized: false,
        saw_activated: false,
        saw_closed: false,
        done: false,
    };
    let _ = display.get_registry(&qh, ());
    eq.roundtrip(&mut app).expect("roundtrip");
    if app.manager.is_none() {
        println!("RESULT ok=false reason=no-manager-global");
        return;
    }

    // Drive the request path once a handle exists: minimize →
    // wait → unminimize+activate → wait → close. A timeout loop
    // keeps the test honest if the compositor stalls.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    let mut phase_at = std::time::Instant::now();
    while !app.done && std::time::Instant::now() < deadline {
        eq.blocking_dispatch(&mut app).expect("dispatch");
        if app.handle.is_none() {
            continue;
        }
        let h = app.handle.as_ref().unwrap();
        match app.phase {
            0 if !app.title.is_empty() => {
                h.set_minimized();
                conn.flush().expect("flush");
                app.phase = 1;
                phase_at = std::time::Instant::now();
                println!("SENT set_minimized");
            }
            1 if app.saw_minimized || phase_at.elapsed().as_secs() > 3 => {
                h.unset_minimized();
                if let Some(seat) = &app.seat {
                    h.activate(seat);
                }
                conn.flush().expect("flush");
                app.phase = 2;
                phase_at = std::time::Instant::now();
                println!("SENT unset_minimized+activate");
            }
            2 if app.saw_activated || phase_at.elapsed().as_secs() > 3 => {
                h.close();
                conn.flush().expect("flush");
                app.phase = 3;
                println!("SENT close");
            }
            _ => {}
        }
    }
    println!(
        "RESULT ok={} minimized={} activated={} closed={} title=\"{}\" appid=\"{}\"",
        app.saw_minimized && app.saw_activated && app.saw_closed,
        app.saw_minimized,
        app.saw_activated,
        app.saw_closed,
        app.title,
        app.app_id
    );
}

// Objects we bind but never listen to still need a handler.
macro_rules! noop {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
noop!(wl_seat::WlSeat, wl_output::WlOutput);
