// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Scripted nested selftest — drives the compositor with
//! synthetic input on a fixed tick schedule so the whole
//! pipeline (clients, focus, protocols, workspaces, dumps)
//! is verifiable without a user present. Enabled with
//! `ODIN_NESTED_TEST=1`; pixel-checkable via the PPM dumps.

use tracing::info;

use crate::compositor::CompState;

/// One tick of the scripted test — called from the nested
/// present loop only when `ODIN_NESTED_TEST` is set.
pub fn tick(tick: u64, comp: &mut Option<CompState>, frame: *mut u8, w: u32, h: u32) {
    if std::env::var("ODIN_NESTED_TEST").as_deref() == Ok("xev") {
        return xev_tick(tick, comp);
    }
    match tick {
        40 => crate::spawn::run_binding("spawn weston-terminal", None),
        120 => crate::spawn::run_binding("spawn weston-terminal", None),
        // Layer-shell smoke test — a real background
        // client exercises configure/ack/placement; the
        // frame dump below shows it when present.
        140 => crate::spawn::run_binding("spawn swaybg -c '#204080'", None),
        // The pointer-protocol test client joins right
        // before the sweep — last-mapped sits on top of
        // the stacking order so its surface is hit. Its
        // CONFINED/RELMOTION prints land in this log.
        300 => crate::spawn::run_binding("spawn target/debug/examples/ptr_proto_test", None),
        // Sacrificial third terminal — the foreign-
        // toplevel test closes its tracked handle at the
        // end; without this the dump's window count
        // would drop below the verified order=2.
        305 => crate::spawn::run_binding("spawn weston-terminal", None),
        // The taskbar-protocol client replays the
        // existing toplevels, then drives minimize /
        // activate / close end-to-end. Prints land in
        // this log like the pointer test's.
        318 => crate::spawn::run_binding("spawn target/debug/examples/ft_proto_test", None),
        220 => {
            if let Some(c) = comp.as_mut() {
                c.toggle_layout();
            }
        }
        // Click into the left terminal → keyboard focus,
        // then type — if glyphs appear the whole input
        // chain (hit-test, focus, wl_keyboard, repaint,
        // frame callback) works.
        // If an X window sits at (0,0) — xeyes in the
        // manual Xwayland checks — clicking it first proves
        // the focus path reaches it before any terminal
        // raise covers the spot.
        258 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_motion(75.0, 50.0);
                c.ptr_button(0x110, true);
                c.ptr_button(0x110, false);
            }
        }
        // EWMH check — after the X-surface click the root
        // property must carry the xeyes window id.
        275 => {
            if let Some(c) = comp.as_mut() {
                crate::spawn::run_binding("spawn xprop -root _NET_ACTIVE_WINDOW", c.x_display);
            }
        }
        295 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_motion(320.0, 400.0);
                c.ptr_button(0x110, true);
                c.ptr_button(0x110, false);
            }
        }
        // Second probe — focusing the Wayland terminal must
        // clear _NET_ACTIVE_WINDOW back to none.
        315 => {
            if let Some(c) = comp.as_mut() {
                crate::spawn::run_binding("spawn xprop -root _NET_ACTIVE_WINDOW", c.x_display);
            }
        }
        // Diagonal pointer sweep — protocol test clients
        // (relative-pointer, constraints, cursor-shape)
        // connect at arbitrary times; an oscillating sweep
        // guarantees their surfaces get entered and motion
        // events flow no matter where or when they map.
        320..=560 => {
            if let Some(c) = comp.as_mut() {
                let t = (tick - 320) as f64;
                // Raster sweep: x zig-zags the full width,
                // y steps down a band every 40 ticks —
                // covers the whole 1280×800 scene so a
                // retiled surface is always crossed.
                let band = (t / 40.0).floor();
                let frac = (t % 40.0) / 40.0;
                let s = if (band as i32) % 2 == 0 {
                    frac
                } else {
                    1.0 - frac
                };
                c.ptr_motion(10.0 + s * 1200.0, 20.0 + band * 110.0);
                // A complete gesture per tick — whenever the
                // sweep crosses the gesture-test client's surface,
                // it sees a full begin/update/end stream. The
                // focus gate delivers only while that surface
                // holds pointer focus.
                use crate::compositor::gestures::Phase;
                c.swipe(Phase::Begin, 0.0, 0.0, 3, false);
                c.swipe(Phase::Update, 4.0, 2.0, 3, false);
                c.swipe(Phase::End, 0.0, 0.0, 3, false);
                c.pinch(Phase::Begin, 0.0, 0.0, 1.0, 0.0, 2, false);
                c.pinch(Phase::Update, 1.0, 0.5, 1.05, 2.0, 2, false);
                c.pinch(Phase::End, 0.0, 0.0, 0.0, 0.0, 2, false);
                // A complete touch point per tick — down grabs
                // the surface under the swept position, motion
                // + up follow on the same slot, frame closes the
                // batch like libinput does. A client quoting the
                // serial for move/resize after up hits the
                // dead-slot check — no stale grab can arm.
                let (tx, ty) = (10.0 + s * 1200.0, 20.0 + band * 110.0);
                c.touch_down(0, tx, ty);
                c.touch_motion(0, tx + 4.0, ty + 2.0);
                c.touch_up(0);
                c.touch_frame();
            }
        }
        280 => {
            if let Some(c) = comp.as_mut() {
                for k in [30u32, 18, 25, 24, 57, 21, 32, 39, 39, 24] {
                    c.key_event(k, true);
                    c.key_event(k, false);
                }
            }
        }
        // Drag & drop test — the client maps the newest
        // (bottom-right stack) window but keeps a 200x200
        // buffer, so its hittable area is (640,400) +
        // 200px. Press inside it, drag across, release —
        // source and destination are the same client.
        561 => crate::spawn::run_binding("spawn target/debug/examples/dnd_proto_test", None),
        580 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_motion(700.0, 500.0);
                c.ptr_button(0x110, true);
            }
        }
        581..=586 => {
            if let Some(c) = comp.as_mut() {
                let t = (tick - 581) as f64 / 5.0;
                c.ptr_motion(700.0 + 60.0 * t, 500.0 + 60.0 * t);
            }
        }
        587 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_button(0x110, false);
            }
        }
        // Workspace check — switch to empty ws 2; the
        // dump below must show no windows (only layer
        // surfaces + backdrop), proving the visibility
        // filter. Switching back restores everything.
        592 => {
            if let Some(c) = comp.as_mut() {
                c.switch_ws(1);
            }
        }
        // Dump while ws 2 is up: the frame must contain
        // no window pixels — the visibility proof.
        596 => {
            dump_ppm(frame, w, h, "nested-test-ws2.ppm");
            info!("nested self-test: empty-ws frame dumped");
        }
        600 => {
            if let Some(c) = comp.as_mut() {
                c.switch_ws(0);
            }
        }
        // Keyboard-shortcuts-inhibit test — the client maps and
        // claims inhibition; then we inject C-A-d (the dump-frame
        // binding). Inhibited, the WHOLE combo must reach its
        // wl_keyboard instead of triggering the binding.
        603 => crate::spawn::run_binding("spawn target/debug/examples/inhibit_proto_test", None),
        608 => {
            if let Some(c) = comp.as_mut() {
                for (k, p) in [
                    (29u32, true),
                    (56, true),
                    (32, true),
                    (32, false),
                    (56, false),
                    (29, false),
                ] {
                    c.key_event(k, p);
                }
            }
        }
        620 => {
            dump_ppm(frame, w, h, "nested-test.ppm");
            info!("nested self-test: frame dumped — exiting");
            std::process::exit(0);
        }
        _ => {}
    }
}

/// X-input delivery test (`ODIN_NESTED_TEST=xev`) — xev is the
/// ONLY client, so every hit lands on it. Its stdout records
/// exactly which X events Xwayland forwarded: EnterNotify proves
/// enter, MotionNotify proves motion, ButtonPress proves click
/// delivery end-to-end through the XWM.
fn xev_tick(tick: u64, comp: &mut Option<CompState>) {
    match tick {
        60 => {
            if let Some(c) = comp.as_mut() {
                crate::spawn::run_binding("spawn xev -geometry 1200x700+40+40", c.x_display);
            }
        }
        // Wait for map+associate, then sweep the pointer in
        // real motion steps and click — xev's event list is the
        // verdict on the whole X input path.
        400..=430 => {
            if let Some(c) = comp.as_mut() {
                let t = (tick - 400) as f64;
                c.ptr_motion(100.0 + t * 30.0, 300.0);
            }
        }
        440 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_motion(400.0, 300.0);
                c.ptr_button(0x110, true);
            }
        }
        441 => {
            if let Some(c) = comp.as_mut() {
                c.ptr_button(0x110, false);
            }
        }
        // Keyboard crosscheck — a KeyPress reaching xev proves
        // Xwayland dispatches its Wayland queue at all; silence
        // on both fronts means the queue itself is starved.
        442 => {
            if let Some(c) = comp.as_mut() {
                c.key_event(45, true); // 'x'
                c.key_event(45, false);
            }
        }
        // The X server's own view — where its window tree says
        // xev sits. Divergence from the surface-local coords we
        // send in enter/motion is what makes Xwayland drop the
        // whole stream silently.
        445 => {
            if let Some(c) = comp.as_mut() {
                crate::spawn::run_binding("spawn xwininfo -root -tree", c.x_display);
                // The X sprite position — Xwayland only moves it
                // from wl_pointer events it actually accepted.
                crate::spawn::run_binding("spawn xdotool getmouselocation", c.x_display);
                // XTEST moves the sprite through the X server
                // itself — if this lands, the X input core works
                // and only the wayland→X translation is dead.
                crate::spawn::run_binding("spawn xdotool mousemove 555 444", c.x_display);
                crate::spawn::run_binding("spawn xdpyinfo -queryExtensions", c.x_display);
                // xset prints "Pointer Control" only when a real
                // pointer device exists in the server.
                crate::spawn::run_binding("spawn xset q", c.x_display);
            }
        }
        455 => {
            if let Some(c) = comp.as_mut() {
                crate::spawn::run_binding("spawn xdotool getmouselocation", c.x_display);
            }
        }
        460 => {
            info!("xev selftest done — exiting");
            std::process::exit(0);
        }
        _ => {}
    }
}

/// Write the nested frame as a PPM (BGRA → RGB) into the
/// state dir — the pixel-verification artifact.
fn dump_ppm(ptr: *mut u8, w: u32, h: u32, name: &str) {
    let dir = format!("{}/.local/state/odin", std::env::var("HOME").unwrap());
    let _ = std::fs::create_dir_all(&dir);
    let px = unsafe { std::slice::from_raw_parts(ptr, (w * h * 4) as usize) };
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in px.chunks(4) {
        ppm.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    std::fs::write(format!("{dir}/{name}"), ppm).unwrap();
}
