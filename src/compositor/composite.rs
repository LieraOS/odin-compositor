// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! The CPU composite pass — scene items painted into a packed
//! BGRA frame. GPU backends share the `scene()` ordering but
//! draw their own way; this file owns the per-pixel path the
//! CPU renderer and every readback/capture consumer uses.

use wayland_server::Resource;

use super::blit::{blit, blit_scaled};
use super::render::SceneItem;
use super::{BufInfo, CompState, SurfaceData};

impl CompState {
    /// Paint the scene into `frame` (packed BGRA, `fw`x`fh`),
    /// output-local for origin `(ox, oy)`. `damage` is a global-
    /// space dirty list — `None` repaints everything; `Some`
    /// limits every blit to the damaged intersections.
    /// `with_cursor` appends the pointer — captures pass `false`
    /// so the compositor cursor never lands in screen content.
    pub fn composite(
        &mut self,
        frame: &mut [u8],
        fw: u32,
        fh: u32,
        ox: i32,
        oy: i32,
        with_cursor: bool,
        damage: Option<&[(i32, i32, i32, i32)]>,
    ) {
        // Damage arrives in global space — translate once to
        // this output's local pixels for every clip test below.
        let local: Vec<(i32, i32, i32, i32)> = damage
            .map(|rs| {
                rs.iter()
                    .map(|&(x, y, w, h)| (x - ox, y - oy, w, h))
                    .collect()
            })
            .unwrap_or_default();
        let clip = damage.map(|_| local.as_slice());
        // The scene order — layers, windows, popups, lock gating —
        // is shared with the GPU backends via scene().
        for item in self.scene() {
            match item {
                SceneItem::Surf { surf, x, y } => {
                    if let Some(sd) = surf.data::<SurfaceData>() {
                        draw_at(frame, fw, fh, sd, x - ox, y - oy, clip);
                    }
                }
                SceneItem::Fill { x, y, w, h, rgba } => {
                    fill_rect(frame, fw, fh, x - ox, y - oy, w, h, rgba, clip);
                }
            }
        }
        // The pointer is a scene element drawn last so nothing
        // covers it — precedence is last-request-wins: a themed
        // wp_cursor_shape image, a client-provided cursor
        // surface, then the default arrow.
        if !with_cursor {
            return;
        }
        // An active drag icon rides the cursor — drawn slightly
        // offset so the pointer tip stays the drop point. Like
        // the cursor itself it never lands in captures.
        if let Some(icon) = self
            .drag
            .as_ref()
            .and_then(|d| d.icon.clone())
            .filter(|s| s.is_alive())
        {
            if let Some(sd) = icon.data::<SurfaceData>() {
                draw_at(
                    frame,
                    fw,
                    fh,
                    sd,
                    self.ptr_pos.0 as i32 - ox + 4,
                    self.ptr_pos.1 as i32 - oy + 4,
                    clip,
                );
            }
        }
        if let Some(img) = &self.cursor_img {
            let (cx, cy) = (
                self.ptr_pos.0 as i32 - ox - img.hx,
                self.ptr_pos.1 as i32 - oy - img.hy,
            );
            // Undamaged cursor region → skip; the frame already
            // shows this cursor where it stands.
            let hits = clip
                .map(|rs| !intersects(cx, cy, img.w as i32, img.h as i32, rs).is_empty())
                .unwrap_or(true);
            if hits {
                crate::cursor::blit_img(frame, fw, fh, img, cx, cy);
            }
            return;
        }
        let cursor_drawn = self
            .cursor_surf
            .as_ref()
            .filter(|s| s.is_alive())
            .map(|s| {
                if let Some(sd) = s.data::<SurfaceData>() {
                    return draw_at(
                        frame,
                        fw,
                        fh,
                        sd,
                        self.ptr_pos.0 as i32 - ox - self.cursor_hotspot.0,
                        self.ptr_pos.1 as i32 - oy - self.cursor_hotspot.1,
                        clip,
                    );
                }
                false
            })
            .unwrap_or(false);
        if !cursor_drawn {
            let (cx, cy) = (self.ptr_pos.0 as i32 - ox, self.ptr_pos.1 as i32 - oy);
            let hits = clip
                .map(|rs| !intersects(cx, cy, 16, 24, rs).is_empty())
                .unwrap_or(true);
            if hits {
                crate::cursor::draw(frame, fw, fh, cx, cy);
            }
        }
    }
}

/// Paint a compositor-owned solid rect (SSD frame parts) into
/// the CPU frame — clipped by the output bounds and, when a
/// damage set rides along, by every dirty rect. Fill colors
/// are opaque, so no alpha math applies.
fn fill_rect(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rgba: [u8; 4],
    clip: Option<&[(i32, i32, i32, i32)]>,
) {
    let rects: Vec<(i32, i32, i32, i32)> = match clip {
        Some(rs) => intersects(x, y, w, h, rs),
        None => vec![(x, y, w, h)],
    };
    for (rx, ry, rw, rh) in rects {
        let x0 = rx.max(0);
        let y0 = ry.max(0);
        let x1 = (rx + rw).min(fw as i32);
        let y1 = (ry + rh).min(fh as i32);
        // Rects fully outside the frame leave x0 >= x1 — clip sets
        // are intersected against surface bounds, never against the
        // output, so an off-screen rect slices backwards and panics.
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for row in y0..y1 {
            let start = (row * fw as i32 + x0) as usize * 4;
            let end = (row * fw as i32 + x1) as usize * 4;
            for px in frame[start..end].chunks_exact_mut(4) {
                px.copy_from_slice(&rgba);
            }
        }
    }
}

/// Draw the shown buffer at explicit output-local coords.
/// Returns false when there is nothing to show.
fn draw_at(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    sd: &SurfaceData,
    dx: i32,
    dy: i32,
    clip: Option<&[(i32, i32, i32, i32)]>,
) -> bool {
    let shown = sd.shown_buf.lock().unwrap();
    let Some(buf) = shown.as_ref() else {
        return false;
    };
    let Some(info) = buf.data::<BufInfo>() else {
        return false;
    };
    let map = info.map.lock().unwrap();
    let src = map.ptr as *const u8;
    let len = map.len as i64;
    // Bounds-check: clients may hand us buffers whose declared
    // geometry runs past the pool's end — reading the unmapped
    // tail SIGBUSes the compositor.
    if src.is_null() || src as isize == -1 {
        return false;
    }
    // wp_viewporter — the committed crop+scale. Identity means
    // the whole buffer at natural size: the plain blit below.
    let vp = *sd.vp.lock().unwrap();
    // Committed wp_alpha_modifier multiplier — None is identity.
    let alpha = sd.alpha.lock().unwrap().unwrap_or(1.0) as f32;
    let (sx, sy, sw, sh) = vp.src.unwrap_or((0.0, 0.0, info.w as f64, info.h as f64));
    let (dw, dh) = vp.dst.unwrap_or((sw.ceil() as i32, sh.ceil() as i32));
    let identity = (sx, sy) == (0.0, 0.0)
        && (sw.ceil() as i32, sh.ceil() as i32) == (dw, dh)
        && (dw, dh) == (info.w, info.h);
    if identity {
        match clip {
            None => blit(frame, fw, fh, src, len, info, dx, dy, None, alpha),
            Some(rects) => {
                // Damaged blit: only the surface regions intersecting
                // a damage rect repaint — undamaged pixels keep the
                // frame's previous content.
                for r in intersects(dx, dy, info.w, info.h, rects) {
                    blit(frame, fw, fh, src, len, info, dx, dy, Some(r), alpha);
                }
            }
        }
    } else {
        match clip {
            None => blit_scaled(
                frame,
                fw,
                fh,
                src,
                len,
                info,
                dx,
                dy,
                (sx, sy, sw, sh),
                (dw, dh),
                None,
                alpha,
            ),
            Some(rects) => {
                for r in intersects(dx, dy, dw, dh, rects) {
                    blit_scaled(
                        frame,
                        fw,
                        fh,
                        src,
                        len,
                        info,
                        dx,
                        dy,
                        (sx, sy, sw, sh),
                        (dw, dh),
                        Some(r),
                        alpha,
                    );
                }
            }
        }
    }
    true
}

/// Intersect a surface's local bbox against the damage clip
/// list — returns only the rects that actually overlap it.
fn intersects(
    dx: i32,
    dy: i32,
    w: i32,
    h: i32,
    rects: &[(i32, i32, i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    rects
        .iter()
        .filter_map(|&(rx, ry, rw, rh)| {
            let x = dx.max(rx);
            let y = dy.max(ry);
            let r = (dx + w).min(rx + rw);
            let b = (dy + h).min(ry + rh);
            (r > x && b > y).then_some((x, y, r - x, b - y))
        })
        .collect()
}
