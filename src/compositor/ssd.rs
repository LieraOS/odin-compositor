// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Server-side decorations — the compositor-drawn frame for
//! toplevels that negotiated (or were assigned) SSD: a
//! titlebar, a thin border, resize grips at every edge and a
//! close button. The client's content rect stays untouched;
//! the frame is pure margin around `sd.pos`, so hit-testing
//! and layout adjustments live entirely on our side.
//!
//! Drawing happens via `SceneItem::Fill` slots — every
//! renderer (CPU composite + Vulkan) paints the same solid
//! quads, so no font or widget toolkit is involved.

use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::Resource;

use super::render::SceneItem;
use super::{CompState, SurfaceData};

/// Titlebar height in surface pixels.
pub(crate) const TITLE_H: i32 = 24;
/// Border thickness on the left/right/bottom edges.
pub(crate) const BORDER: i32 = 1;
/// Invisible resize-grip thickness — wider than the drawn
/// border so edge/corner grabs are comfortable to hit.
pub(crate) const GRIP: i32 = 6;
/// Close-button square size, centered in the titlebar.
pub(crate) const BTN: i32 = 16;

/// The frame's part under the pointer — drives the action a
/// press starts: move (titlebar), close (button), resize
/// (every edge and corner).
#[derive(Clone, Copy)]
pub(crate) enum SsdZone {
    Title,
    Close,
    Bottom,
    Left,
    Right,
    BottomLeft,
    BottomRight,
}

impl SsdZone {
    /// Map the zone to the protocol resize edge a grab needs.
    pub(crate) fn edge(&self) -> xdg_toplevel::ResizeEdge {
        use xdg_toplevel::ResizeEdge as E;
        match self {
            Self::Bottom => E::Bottom,
            Self::Left => E::Left,
            Self::Right => E::Right,
            Self::BottomLeft => E::BottomLeft,
            Self::BottomRight => E::BottomRight,
            _ => E::None,
        }
    }
}

/// Frame palette — BGRA premultiplied, the same byte order
/// every renderer consumes.
const BORDER_C: [u8; 4] = [30, 30, 32, 255];
const TITLE_ON: [u8; 4] = [148, 100, 48, 255]; // focused: warm blue
const TITLE_OFF: [u8; 4] = [64, 64, 66, 255]; // unfocused: flat gray
const BTN_C: [u8; 4] = [64, 72, 168, 255]; // close: muted red (BGR)

impl CompState {
    /// Whether this surface wears a compositor frame right now:
    /// the negotiated/assigned SSD flag plus the runtime rules —
    /// fullscreen drops its frame, XWayland keeps its own,
    /// non-toplevels never had one.
    pub(crate) fn ssd_on(&self, sd: &SurfaceData) -> bool {
        if !sd.ssd.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        if sd.xwin.lock().unwrap().is_some() {
            return false; // X11 decorations are Xwm's business
        }
        !sd.tl_states
            .lock()
            .unwrap()
            .contains(&xdg_toplevel::State::Fullscreen)
    }

    /// The decorated extent in global space — the content rect
    /// (position + presented size) grown by the frame margins.
    pub(crate) fn ssd_rect(&self, sd: &SurfaceData) -> (i32, i32, i32, i32) {
        let (px, py) = *sd.pos.lock().unwrap();
        let (w, h) = self.surf_size(sd);
        (
            px - BORDER,
            py - TITLE_H - BORDER,
            w + BORDER * 2,
            h + TITLE_H + BORDER * 2,
        )
    }

    /// The close button's rect inside the decorated extent —
    /// right-aligned in the titlebar band, vertically centered.
    fn ssd_btn(ext: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
        let (dx, dy, dw, _) = ext;
        (
            dx + dw - BORDER - BTN - 4,
            dy + (TITLE_H - BTN) / 2,
            BTN,
            BTN,
        )
    }

    /// Which window's frame (if any) owns this global point —
    /// topmost first, same stacking order the scene draws.
    /// Returns the surface plus the frame zone, or `None` when
    /// the point is over client content or nothing framed.
    pub(crate) fn ssd_pick(&self, x: f64, y: f64) -> Option<(WlSurface, SsdZone)> {
        for surf in self.order.iter().rev() {
            let Some(sd) = surf.data::<SurfaceData>() else {
                continue;
            };
            if !self.ssd_on(sd)
                || sd.minimized.load(std::sync::atomic::Ordering::SeqCst)
                || !self.ws_visible(surf)
            {
                continue;
            }
            let (dx, dy, dw, dh) = self.ssd_rect(sd);
            let (px, py) = (x as i32, y as i32);
            if px < dx || px >= dx + dw || py < dy || py >= dy + dh {
                continue;
            }
            // Inside the decorated extent — the content itself
            // starts below the titlebar and inside the border.
            let (cx, cy) = *sd.pos.lock().unwrap();
            let (cw, ch) = self.surf_size(sd);
            let in_content = px >= cx && px < cx + cw && py >= cy && py < cy + ch;
            if in_content {
                continue; // the client's own area — not our frame
            }
            // Titlebar band — the button wins over the plain
            // title grip.
            let (bx, by, bw, bh) = Self::ssd_btn((dx, dy, dw, dh));
            if px >= bx && px < bx + bw && py >= by && py < by + bh {
                return Some((surf.clone(), SsdZone::Close));
            }
            if py < cy {
                return Some((surf.clone(), SsdZone::Title));
            }
            // Side borders run the content's full height; the
            // bottom strip's outer GRIP pixels count as corners
            // so diagonal resizes are as easy as edge drags.
            let left = px < cx;
            let right = px >= cx + cw;
            let near_bottom = py >= cy + ch - GRIP;
            let zone = match (left, right, near_bottom) {
                (true, _, true) => SsdZone::BottomLeft,
                (_, true, true) => SsdZone::BottomRight,
                (true, _, _) => SsdZone::Left,
                (_, true, _) => SsdZone::Right,
                _ => SsdZone::Bottom,
            };
            return Some((surf.clone(), zone));
        }
        None
    }

    /// Pointer press on compositor chrome — raise + focus the
    /// window, then act per zone: titlebar starts a move grab,
    /// the close button closes the toplevel, every edge or
    /// corner starts a resize grab. Returns `true` when the
    /// press landed on a frame and was consumed here.
    ///
    /// Surfaces drawn above every frame — popups, X11
    /// override-redirects, layers, IME popups — keep their
    /// clicks: when the topmost hit isn't frame territory the
    /// dispatch defers to the normal client path.
    pub(crate) fn ssd_button(&mut self, x: f64, y: f64) -> bool {
        let over_chrome = match self.hit(x, y) {
            None => true,
            Some((s, _, _)) => {
                !(self.popups.iter().any(|p| p.surf.id() == s.id())
                    || self.xpopups.iter().any(|p| p.id() == s.id())
                    || self.layer_index(&s).is_some()
                    || self.im_popups.iter().any(|p| p.surf.id() == s.id()))
            }
        };
        if !over_chrome {
            return false;
        }
        let Some((surf, zone)) = self.ssd_pick(x, y) else {
            return false;
        };
        self.raise(&surf);
        self.set_kbd_focus(Some(surf.clone()));
        let Some(sd) = surf.data::<SurfaceData>() else {
            return true; // chrome swallowed the press regardless
        };
        match zone {
            SsdZone::Close => {
                if let Some(tl) = sd.xdg_tl.lock().unwrap().as_ref() {
                    tl.close();
                }
            }
            SsdZone::Title => {
                let (wx, wy) = *sd.pos.lock().unwrap();
                self.grab = Some(super::seat::Grab {
                    surf,
                    kind: super::seat::GrabKind::Move {
                        dx: x - wx as f64,
                        dy: y - wy as f64,
                    },
                    touch: None,
                });
            }
            zone => {
                let (wx, wy) = *sd.pos.lock().unwrap();
                let (ww, wh) = self.surf_size(sd);
                // Resizing flag on the toplevel like the
                // client-driven path — the grab release sends
                // the final non-Resizing configure.
                let mut st = sd.tl_states.lock().unwrap();
                let resizing = xdg_toplevel::State::Resizing;
                if !st.contains(&resizing) {
                    st.push(resizing);
                }
                drop(st);
                self.grab = Some(super::seat::Grab {
                    surf,
                    kind: super::seat::GrabKind::Resize {
                        edges: zone.edge(),
                        start: (wx, wy, ww, wh),
                        anchor: (x, y),
                    },
                    touch: None,
                });
            }
        }
        true
    }

    /// The frame's draw slots, emitted just before the window's
    /// own surface item so content paints over the frame's
    /// center. Three quads: the whole decorated extent in
    /// border color (the visible border is what the other two
    /// rects leave uncovered), the titlebar band in the
    /// focus-dependent color, and the close button.
    pub(crate) fn ssd_fills(&self, surf: &WlSurface) -> Vec<SceneItem> {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return Vec::new();
        };
        if !self.ssd_on(sd) {
            return Vec::new();
        }
        let (dx, dy, dw, dh) = self.ssd_rect(sd);
        let focused = self.kbd_focus.as_ref().is_some_and(|f| f.id() == surf.id());
        let title_c = if focused { TITLE_ON } else { TITLE_OFF };
        let (bx, by, bw, bh) = Self::ssd_btn((dx, dy, dw, dh));
        vec![
            SceneItem::Fill {
                x: dx,
                y: dy,
                w: dw,
                h: dh,
                rgba: BORDER_C,
            },
            SceneItem::Fill {
                x: dx,
                y: dy,
                w: dw,
                h: TITLE_H,
                rgba: title_c,
            },
            SceneItem::Fill {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                rgba: BTN_C,
            },
        ]
    }
}
