// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_subcompositor / wl_subsurface — core-protocol subsurfaces.
//!
//! A subsurface is a plain wl_surface with a parent: it draws at
//! parent_origin + set_position, stacked relative to the parent's
//! own buffer by the parent's `sub_stack` list (Root marker =
//! the parent's pixels). `set_position`/`place_above`/
//! `place_below` are double-buffered — they apply at commit.
//!
//! Sync semantics per spec: a subsurface starts SYNCHRONIZED —
//! its commits cache until the parent applies a commit, which
//! is what makes e.g. mpv's video plane and its UI overlay
//! composite atomically. `set_desync` switches to immediate
//! commits; `set_sync` back. Desync parents do NOT break the
//! chain upward — a sync'd child still waits for ITS parent.
//!
//! Uses: Firefox plugin surfaces, mpv/GStreamer video planes,
//! GTK4 video widgets, Qt multimedia — all of which bind
//! wl_subcompositor unconditionally and previously got no global.

use wayland_server::protocol::{wl_subcompositor, wl_subsurface, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, GlobalDispatch, New, Resource};

use super::render::SceneItem;
use super::{CompState, SubItem, SurfaceData};

impl GlobalDispatch<wl_subcompositor::WlSubcompositor, ()> for CompState {
    fn bind(
        _state: &mut Self,
        _handle: &wayland_server::DisplayHandle,
        _client: &Client,
        resource: New<wl_subcompositor::WlSubcompositor>,
        _global_data: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init::<wl_subcompositor::WlSubcompositor, ()>(resource, ());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for CompState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _sub: &wl_subcompositor::WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _dhandle: &wayland_server::DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_subcompositor::Request::GetSubsurface {
                id,
                surface,
                parent,
            } => {
                // bad_surface cases per spec: the surface already
                // carries a role (incl. already being a subsurface),
                // or parent == surface. Nesting is legal — the
                // parent may itself be a subsurface.
                let invalid = match (surface.data::<SurfaceData>(), parent.data::<SurfaceData>()) {
                    (Some(sd), Some(_psd)) => {
                        surface.id() == parent.id()
                            || sd.sub_parent.lock().unwrap().is_some()
                            || sd.xdg_tl.lock().unwrap().is_some()
                            || sd.xdg_surf.lock().unwrap().is_some()
                            || sd.xwin.lock().unwrap().is_some()
                            || sd.is_cursor.load(std::sync::atomic::Ordering::SeqCst)
                            || _state.layer_index(&surface).is_some()
                            || _state.popups.iter().any(|p| p.surf.id() == surface.id())
                    }
                    _ => true,
                };
                if invalid {
                    surface.post_error(
                        wl_subcompositor::Error::BadSurface,
                        "surface already has a role",
                    );
                } else if let (Some(sd), Some(psd)) =
                    (surface.data::<SurfaceData>(), parent.data::<SurfaceData>())
                {
                    // New subsurfaces start synchronized and on top
                    // of the parent's stack (spec defaults).
                    *sd.sub_parent.lock().unwrap() = Some(parent.clone());
                    sd.sub_sync.store(true, std::sync::atomic::Ordering::SeqCst);
                    let mut stack = psd.sub_stack.lock().unwrap();
                    // The Root marker — the slot where the parent's
                    // own buffer composites — must exist before any
                    // child so place_below(parent) has a target.
                    if stack.is_empty() {
                        stack.push(SubItem::Root);
                    }
                    stack.push(SubItem::Child(surface.clone()));
                }
                // The subsurface object's udata IS its wl_surface —
                // dispatch resolves it back without a lookup table.
                init.init::<wl_subsurface::WlSubsurface, wl_surface::WlSurface>(
                    id,
                    surface.clone(),
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, wl_surface::WlSurface> for CompState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        subsurface: &wl_subsurface::WlSubsurface,
        request: wl_subsurface::Request,
        _data: &wl_surface::WlSurface,
        _dhandle: &wayland_server::DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let _ = init;
        // The subsurface object's udata is its wl_surface.
        let Some(surf) = subsurface.data::<wl_surface::WlSurface>() else {
            return;
        };
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        match request {
            wl_subsurface::Request::SetPosition { x, y } => {
                *sd.sub_pending_pos.lock().unwrap() = Some((x, y));
            }
            wl_subsurface::Request::PlaceAbove { sibling } => {
                *sd.sub_pending_place.lock().unwrap() = Some((sibling.id(), true));
            }
            wl_subsurface::Request::PlaceBelow { sibling } => {
                *sd.sub_pending_place.lock().unwrap() = Some((sibling.id(), false));
            }
            wl_subsurface::Request::SetSync => {
                sd.sub_sync.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            wl_subsurface::Request::SetDesync => {
                sd.sub_sync
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            wl_subsurface::Request::Destroy => {
                // Detach from the parent's stack; the surface
                // itself returns to a plain role-less surface.
                if let Some(parent) = sd.sub_parent.lock().unwrap().take() {
                    if let Some(psd) = parent.data::<SurfaceData>() {
                        psd.sub_stack.lock().unwrap().retain(|it| match it {
                            SubItem::Child(c) => c.id() != surf.id(),
                            _ => true,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

impl CompState {
    /// Apply the cached state of a committed surface — position
    /// and restack requests are double-buffered like the attach.
    pub(crate) fn sub_apply_state(&mut self, surf: &wl_surface::WlSurface) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        if let Some(p) = sd.sub_pending_pos.lock().unwrap().take() {
            *sd.sub_pos.lock().unwrap() = p;
        }
        if let Some((sib_id, above)) = sd.sub_pending_place.lock().unwrap().take() {
            let parent = sd.sub_parent.lock().unwrap().clone();
            if let Some(parent) = parent {
                if let Some(psd) = parent.data::<SurfaceData>() {
                    let mut stack = psd.sub_stack.lock().unwrap();
                    stack.retain(|it| match it {
                        SubItem::Child(c) => c.id() != surf.id(),
                        _ => true,
                    });
                    let entry = SubItem::Child(surf.clone());
                    // place_above(sibling): insert directly after the
                    // sibling; place_below: directly before. A sibling
                    // id matching the parent itself means the Root
                    // marker. Unknown siblings (destroyed) fall back
                    // to the top.
                    let pos = stack.iter().position(|it| match it {
                        SubItem::Root => sib_id == parent.id(),
                        SubItem::Child(c) => c.id() == sib_id,
                    });
                    match (pos, above) {
                        (Some(i), true) => stack.insert(i + 1, entry),
                        (Some(i), false) => stack.insert(i, entry),
                        (None, true) => stack.push(entry),
                        (None, false) => stack.insert(0, entry),
                    }
                }
            }
        }
    }

    /// Flush one level of sync'd subsurface children after a
    /// parent's commit applied — each child that cached a commit
    /// now applies it, recursively releasing its own children.
    pub(crate) fn sub_flush_children(&mut self, surf: &wl_surface::WlSurface) {
        let Some(sd) = surf.data::<SurfaceData>() else {
            return;
        };
        let kids: Vec<wl_surface::WlSurface> = sd
            .sub_stack
            .lock()
            .unwrap()
            .iter()
            .filter_map(|it| match it {
                SubItem::Child(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        for kid in kids {
            let Some(ksd) = kid.data::<SurfaceData>() else {
                continue;
            };
            if !ksd
                .sub_cached
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                continue;
            }
            Self::commit_apply(self, &kid, ksd);
        }
    }
}

impl CompState {
    /// Expand a scene item with its subsurface children — the
    /// parent's sub_stack orders children around the Root marker
    /// (the parent's own pixels). Children land at
    /// parent_buffer_origin + sub_pos + their own offset, and
    /// their own children recurse. Dead parents/surfaces skip.
    pub(crate) fn push_with_subs(&self, out: &mut Vec<SceneItem>, it: SceneItem) {
        // Compositor-owned fill slots (SSD chrome) have no
        // subsurface stack — they pass through untouched.
        let SceneItem::Surf { surf, x, y } = &it else {
            out.push(it);
            return;
        };
        let Some(sd) = surf.data::<SurfaceData>() else {
            out.push(it);
            return;
        };
        let stack = sd.sub_stack.lock().unwrap().clone();
        if stack.is_empty() {
            out.push(it);
            return;
        }
        let mut seen_root = false;
        let mut below: Vec<SceneItem> = Vec::new();
        let mut above: Vec<SceneItem> = Vec::new();
        for entry in stack {
            match entry {
                SubItem::Root => seen_root = true,
                SubItem::Child(c) => {
                    let (bx, by) = (*x, *y);
                    let target = if seen_root { &mut above } else { &mut below };
                    self.push_sub(&c, bx, by, target);
                }
            }
        }
        out.extend(below);
        out.push(it);
        out.extend(above);
    }

    /// Emit one subsurface at its parent's origin plus its offset,
    /// recursing into its own sub_stack.
    fn push_sub(&self, c: &wl_surface::WlSurface, bx: i32, by: i32, out: &mut Vec<SceneItem>) {
        if !c.is_alive() {
            return;
        }
        let Some(csd) = c.data::<SurfaceData>() else {
            return;
        };
        if !csd.mapped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let (ox, oy) = *csd.sub_pos.lock().unwrap();
        let (sx, sy) = *csd.offset.lock().unwrap();
        self.push_with_subs(
            out,
            SceneItem::Surf {
                surf: c.clone(),
                x: bx + ox + sx,
                y: by + oy + sy,
            },
        );
    }

    /// Hit-test one band of a parent's subsurface stack —
    /// `above` picks children after the Root marker (drawn over
    /// the parent's buffer), `false` the children before it.
    /// Tests topmost-first recursively so nested subsurfaces
    /// resolve to the deepest live surface, honoring each
    /// child's own input region and the inhibitor gate.
    pub(crate) fn hit_subs(
        &self,
        parent: &wl_surface::WlSurface,
        bx: i32,
        by: i32,
        x: f64,
        y: f64,
        above: bool,
    ) -> Option<(wl_surface::WlSurface, f64, f64)> {
        let sd = parent.data::<SurfaceData>()?;
        let stack = sd.sub_stack.lock().unwrap().clone();
        if stack.is_empty() {
            return None;
        }
        let mut seen_root = false;
        let mut kids: Vec<&wl_surface::WlSurface> = Vec::new();
        for it in &stack {
            match it {
                super::SubItem::Root => seen_root = true,
                super::SubItem::Child(c) => {
                    if seen_root == above {
                        kids.push(c);
                    }
                }
            }
        }
        for c in kids.iter().rev() {
            if !c.is_alive() || !self.input_allowed(c.client().as_ref()) {
                continue;
            }
            let Some(csd) = c.data::<SurfaceData>() else {
                continue;
            };
            let (ox, oy) = *csd.sub_pos.lock().unwrap();
            let (sx, sy) = *csd.offset.lock().unwrap();
            let cx = bx + ox + sx;
            let cy = by + oy + sy;
            // A child's own above-band children still outrank it.
            if let Some(r) = self.hit_subs(c, cx, cy, x, y, true) {
                return Some(r);
            }
            let (w, h) = self.surf_size(csd);
            if x >= cx as f64
                && x < (cx + w) as f64
                && y >= cy as f64
                && y < (cy + h) as f64
                && Self::in_region(csd, x - cx as f64, y - cy as f64)
            {
                return Some(((*c).clone(), x - cx as f64, y - cy as f64));
            }
            if let Some(r) = self.hit_subs(c, cx, cy, x, y, false) {
                return Some(r);
            }
        }
        None
    }
}
