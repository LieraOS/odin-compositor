// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Interactive move/resize grabs — while the pointer is held
//! down after a move/resize request, motion drives the window
//! geometry directly instead of reaching clients as events.

use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::Resource;

use super::seat::GrabKind;
use super::{CompState, SurfaceData};

impl CompState {
    pub(crate) fn grab_motion(&mut self, x: f64, y: f64) -> bool {
        enum K {
            Move(f64, f64),
            Resize(xdg_toplevel::ResizeEdge, (i32, i32, i32, i32), (f64, f64)),
        }
        let (surf, kind) = match self.grab.as_ref() {
            Some(g) if g.surf.is_alive() => (
                g.surf.clone(),
                match &g.kind {
                    GrabKind::Move { dx, dy } => K::Move(*dx, *dy),
                    GrabKind::Resize {
                        edges,
                        start,
                        anchor,
                    } => K::Resize(*edges, *start, *anchor),
                },
            ),
            Some(_) => {
                self.grab = None;
                return false;
            }
            None => return false,
        };
        let Some(sd) = surf.data::<SurfaceData>() else {
            self.grab = None;
            return false;
        };
        match kind {
            K::Move(dx, dy) => {
                let pos = ((x - dx) as i32, (y - dy) as i32);
                *sd.pos.lock().unwrap() = pos;
                // Forensics: a grabbed window whose footprint
                // leaves every output is the "window vanished"
                // report — surface it in the log with the numbers.
                // The window rect is pos + its window geometry
                // (or the shown buffer when no geometry is set).
                let geom = *sd.win_geom.lock().unwrap();
                let (ww, wh) = if geom.2 > 0 && geom.3 > 0 {
                    (geom.2, geom.3)
                } else {
                    sd.shown_buf
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|b| b.data::<crate::compositor::BufInfo>())
                        .map(|bi| (bi.w, bi.h))
                        .unwrap_or((32, 32))
                };
                let on_any = self.layout.outputs.values().any(|o| {
                    let (ow, oh) = (o.mode.0 as i32, o.mode.1 as i32);
                    pos.0 < o.x + ow && pos.0 + ww > o.x && pos.1 < o.y + oh && pos.1 + wh > o.y
                });
                if !on_any {
                    tracing::warn!(?pos, ?ww, ?wh, dx, dy, "grab moved window off all outputs");
                }
                // A dragged window invalidates its whole old and
                // new footprint — client damage never covers that.
                self.damage_everything();
                // Popups follow their parent automatically at draw.
                // X windows: the X side owns its own coordinates —
                // push our placement back or it desyncs.
                if let Some(wid) = *sd.xwin.lock().unwrap() {
                    if let Some(xw) = self.xwins.get_mut(&wid) {
                        xw.x = pos.0;
                        xw.y = pos.1;
                    }
                    // One pending Configure per window — raw
                    // motion at 1kHz would otherwise queue ~17
                    // requests per frame, and apply_requests
                    // writes them ALL into the X connection each
                    // tick: X11 back-pressure stalls the whole
                    // single-threaded loop (video freezes while
                    // dragging). Only the latest position matters.
                    self.xreqs.retain(
                        |r| !matches!(r, crate::xwm::XReq::Configure { win: w, .. } if *w == wid),
                    );
                    self.xreqs.push(crate::xwm::XReq::Configure {
                        win: wid,
                        x: pos.0,
                        y: pos.1,
                        w: None,
                        h: None,
                    });
                }
            }
            K::Resize(edges, start, anchor) => {
                let (sx, sy, sw, sh) = start;
                let dx = (x - anchor.0) as i32;
                let dy = (y - anchor.1) as i32;
                use xdg_toplevel::ResizeEdge as E;
                let mut nx = sx;
                let mut ny = sy;
                let mut nw = sw;
                let mut nh = sh;
                if matches!(edges, E::Left | E::TopLeft | E::BottomLeft) {
                    nx = sx + dx;
                    nw = sw - dx;
                }
                if matches!(edges, E::Right | E::TopRight | E::BottomRight) {
                    nw = sw + dx;
                }
                if matches!(edges, E::Top | E::TopLeft | E::TopRight) {
                    ny = sy + dy;
                    nh = sh - dy;
                }
                if matches!(edges, E::Bottom | E::BottomLeft | E::BottomRight) {
                    nh = sh + dy;
                }
                // Clamp to a sane minimum — negative sizes make
                // clients allocate absurd buffers. The client's
                // own set_min_size/set_max_size bounds apply on
                // top; zero means unset.
                let (mw, mh) = *sd.min_size.lock().unwrap();
                let (xw2, xh2) = *sd.max_size.lock().unwrap();
                let min_w = mw.max(64);
                let min_h = mh.max(64);
                nw = nw.max(min_w);
                nh = nh.max(min_h);
                if xw2 > 0 {
                    nw = nw.min(xw2);
                }
                if xh2 > 0 {
                    nh = nh.min(xh2);
                }
                // A left/top edge clamps the ORIGIN too — the
                // fixed right/bottom corner must not drift.
                if matches!(edges, E::Left | E::TopLeft | E::BottomLeft) {
                    nx = sx + sw - nw;
                }
                if matches!(edges, E::Top | E::TopLeft | E::TopRight) {
                    ny = sy + sh - nh;
                }
                *sd.pos.lock().unwrap() = (nx, ny);
                if let Some(wid) = *sd.xwin.lock().unwrap() {
                    // X windows have no xdg configure — the new
                    // rect goes straight over the wire and the
                    // ConfigureNotify syncs our bookkeeping.
                    if let Some(xw) = self.xwins.get_mut(&wid) {
                        xw.x = nx;
                        xw.y = ny;
                        xw.w = nw as u32;
                        xw.h = nh as u32;
                    }
                    // Same coalescing as the move grab — raw
                    // 1kHz motion must not flood the X request
                    // queue; only the newest rect counts.
                    self.xreqs.retain(
                        |r| !matches!(r, crate::xwm::XReq::Configure { win: w, .. } if *w == wid),
                    );
                    self.xreqs.push(crate::xwm::XReq::Configure {
                        win: wid,
                        x: nx,
                        y: ny,
                        w: Some(nw as u32),
                        h: Some(nh as u32),
                    });
                    self.damage_everything();
                } else {
                    // Don't configure per raw event — 1kHz motion
                    // would flood the client with xdg configures
                    // (each expecting an ack+commit roundtrip).
                    // Stash the newest target; flush_motion sends
                    // ONE configure per frame — the pacing a
                    // client can keep up with.
                    self.rs_pending = Some((surf.clone(), nw, nh));
                }
            }
        }
        true
    }

    /// Release the active grab. A native xdg resize owes the
    /// client a final configure WITHOUT the Resizing state —
    /// omitting it leaves the toplevel flagged Resizing (stuck
    /// resize cursor, wrong client behavior). The coalesced
    /// target size goes
    /// out inside this single configure — no extra roundtrip.
    pub(crate) fn grab_end(&mut self) {
        let Some(g) = self.grab.take() else {
            return;
        };
        if let GrabKind::Move { .. } = g.kind {
            // The drop output's policy takes over — folding into
            // a tiling grid or restoring the natural floating
            // geometry is decided by where the window landed.
            self.drop_policy(&g.surf);
            return;
        }
        let GrabKind::Resize { .. } = g.kind else {
            return;
        };
        let Some(sd) = g.surf.data::<SurfaceData>() else {
            return;
        };
        if sd.xwin.lock().unwrap().is_some() {
            return; // X windows settle via ConfigureNotify
        }
        let (w, h) = match self.rs_pending.take() {
            Some((s, w, h)) if s.id() == g.surf.id() => (w, h),
            _ => *sd.cfg_size.lock().unwrap(),
        };
        crate::compositor::xdg::configure_tl(self, sd, w, h);
    }
}
