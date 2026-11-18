// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_surface request dispatch — attach/damage/commit handling.
//! Split from `surface.rs` to keep files under the size budget;
//! SurfaceData and the commit semantics live there.

use wayland_server::protocol::{wl_callback, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};
impl Dispatch<wl_surface::WlSurface, SurfaceData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        surf: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                *data.pending_buf.lock().unwrap() = Some(buffer);
            }
            // wl_surface.damage is surface-local space (post-
            // viewport); damage_buffer is buffer space — kept
            // apart so the viewport transform can apply each.
            wl_surface::Request::Damage {
                x,
                y,
                width,
                height,
            } => {
                data.damage.lock().unwrap().push((x, y, width, height));
            }
            wl_surface::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                data.buf_damage.lock().unwrap().push((x, y, width, height));
            }
            wl_surface::Request::Offset { x, y } => {
                // v5+: replaces negative attach offsets — accumulate
                // so commit-time placement sees the final value.
                *data.offset.lock().unwrap() = (x, y);
            }
            wl_surface::Request::SetInputRegion { region } => {
                // Snapshot the region's rect union — protocol
                // semantics copy it at request time, later edits
                // to the region object don't leak through.
                *data.input_region.lock().unwrap() = region.map(|r| {
                    r.data::<super::shm::RegionData>()
                        .map(|d| d.rects.lock().unwrap().clone())
                        .unwrap_or_default()
                });
            }
            wl_surface::Request::SetOpaqueRegion { .. } => {
                // Rendering hint only — the CPU/Vulkan paths don't
                // consume it yet; ignoring is spec-legal.
            }
            wl_surface::Request::Commit => {
                // Synchronized subsurfaces HOLD their commit —
                // the state caches until the parent surface's
                // next commit applies it (spec default is sync;
                // that's what makes e.g. a video plane and its
                // UI overlay update atomically).
                if data.sub_parent.lock().unwrap().is_some()
                    && data.sub_sync.load(std::sync::atomic::Ordering::SeqCst)
                {
                    data.sub_cached
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                Self::commit_apply(state, surf, data);
            }
            wl_surface::Request::Frame { callback } => {
                // Queue on the surface — `done` fires from
                // frame_done() after the frame that composites it
                // is presented. Per-surface queues keep pacing
                // honest per client.
                let cb = init.init::<wl_callback::WlCallback, ()>(callback, ());
                data.frames.lock().unwrap().push(cb);
            }
            wl_surface::Request::Destroy => {
                state.damage_everything();
                // A subsurface leaves its parent's stack — the
                // parent's rendering must not hold a dead child.
                if let Some(p) = data.sub_parent.lock().unwrap().take() {
                    if let Some(psd) = p.data::<SurfaceData>() {
                        psd.sub_stack.lock().unwrap().retain(|it| match it {
                            super::SubItem::Child(c) => c.id() != surf.id(),
                            _ => true,
                        });
                    }
                }
                for fb in data.present_fbs.lock().unwrap().drain(..) {
                    if fb.is_alive() {
                        fb.discarded();
                    }
                }
                if let Some(old) = data.shown_buf.lock().unwrap().take() {
                    if old.is_alive() {
                        state.pending_release.push(old);
                    }
                }
                // Explicit-sync teardown — a dying surface still
                // owes its release point a signal (the client's
                // buffer pool waits on it) and a pending stash
                // a cleanup pass.
                if let Some(pt) = data.release_of.lock().unwrap().take() {
                    state.tsig_pending.push(pt);
                }
                if let Some(st) = data.sync_stash.lock().unwrap().take() {
                    if st.buf.is_alive() {
                        state.pending_release.push(st.buf);
                    }
                    if let Some(r) = st.rel {
                        state.tsig_pending.push(r);
                    }
                }
                state.sync_waiters.retain(|s| s.id() != surf.id());
                state.ft_closed(surf);
                state.order.retain(|s| s.id() != surf.id());
                state
                    .popups
                    .retain(|p| p.surf.id() != surf.id() && p.parent.id() != surf.id());
                state.remove_layer(surf);
                if state
                    .focus
                    .as_ref()
                    .map(|f| f.id() == surf.id())
                    .unwrap_or(false)
                {
                    state.focus = None;
                }
                if state
                    .kbd_focus
                    .as_ref()
                    .map(|f| f.id() == surf.id())
                    .unwrap_or(false)
                {
                    state.kbd_focus = None;
                }
                if state
                    .cursor_surf
                    .as_ref()
                    .map(|s| s.id() == surf.id())
                    .unwrap_or(false)
                {
                    state.cursor_surf = None;
                    state.cursor_gen += 1;
                }
                // A vacated tile is reclaimed by the survivors.
                state.apply_layout();
            }
            _ => {}
        }
    }
}

impl CompState {
    /// The apply half of wl_surface.commit — pending attach,
    /// viewport/alpha/content/tearing state, damage drain and
    /// the map-path pick. Kept as its own method so sync'd
    /// subsurfaces can run the same apply when their parent's
    /// commit releases them (sub_flush_children).
    pub(crate) fn commit_apply(
        state: &mut CompState,
        surf: &wl_surface::WlSurface,
        data: &SurfaceData,
    ) {
        // Any commit changes the scene — mark for render.
        state.dirty = true;
        // Committed content version — GPU texture caches
        // re-upload buffers rewritten under the same id.
        data.commit_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // New pixels for the live cursor surface — the
        // hardware cursor plane re-uploads on the bump.
        if state
            .cursor_surf
            .as_ref()
            .is_some_and(|s| s.id() == surf.id())
        {
            state.cursor_gen += 1;
        }
        // Only a pending attach replaces the shown buffer;
        // commits without attach keep it (damage-only).
        let pending = data.pending_buf.lock().unwrap().take();
        // Sync gate — explicit (wp_linux_drm_syncobj) validates
        // the point contract and may hold the attach until its
        // acquire fence signals; implicit dmabuf fences stash
        // too. Either way the returned buffer may show NOW and
        // carries the release point the swap installs below.
        let (pending, rel) = state.commit_gate(surf, data, pending);
        let new_attach = pending.is_some();
        if let Some(next) = pending {
            let next_id = next.as_ref().map(|b| b.id());
            // Queue the buffer we no longer show for a
            // deferred release — without wl_buffer.release
            // the client's pool exhausts after two commits
            // ("all buffers are held by the server"), but
            // releasing at attach time let dmabuf clients
            // repaint a buffer whose pixels still sat in a
            // frame on the CRTC (sporadic flicker). The
            // release goes out after a replacement frame
            // has actually been presented (frame_done).
            let prev = std::mem::replace(&mut *data.shown_buf.lock().unwrap(), next);
            // The displaced release point follows the same
            // deferred-signal staging — GPU provably done.
            if let Some(pt) = data.release_of.lock().unwrap().take() {
                state.tsig_pending.push(pt);
            }
            *data.release_of.lock().unwrap() = rel;
            if let Some(old) = prev {
                if old.is_alive() && next_id != Some(old.id()) {
                    state.pending_release.push(old);
                }
            }
        }
        // Pending viewport state applies with the commit —
        // double-buffered like the attach itself.
        *data.vp.lock().unwrap() = *data.vp_pending.lock().unwrap();
        // Alpha multiplier + content hint — same
        // double-buffered discipline as the viewport.
        if let Some(a) = data.alpha_pending.lock().unwrap().take() {
            *data.alpha.lock().unwrap() = if a == 1.0 { None } else { Some(a) };
        }
        if let Some(c) = data.ctype_pending.lock().unwrap().take() {
            *data.content_type.lock().unwrap() = c;
        }
        // Tearing hint — same double-buffered discipline.
        if let Some(t) = data.tearing_pending.lock().unwrap().take() {
            *data.tearing.lock().unwrap() = t;
        }
        // Toplevel icon — set_icon already snapshotted the pixel
        // copies; the commit only swaps the pointer.
        if let Some(p) = data.icon_pending.lock().unwrap().take() {
            *data.icon.lock().unwrap() = p.unwrap_or_default();
        }
        // Fractional scale: re-announce when the surface's
        // effective output scale moved (output hop or an
        // output-management scale apply).
        super::fractional_scale::refresh(state, surf, data);
        // Drain the client's damage into frame space —
        // an attach without any damage rect means the
        // whole surface changed (spec default).
        let attached = data.shown_buf.lock().unwrap().is_some();
        let surf_dmg: Vec<(i32, i32, i32, i32)> = std::mem::take(&mut *data.damage.lock().unwrap());
        let mut dmg = surf_dmg.clone();
        // damage_buffer rects are buffer-space — a viewport
        // maps them through src-crop + dst-scale into the
        // same surface-local space the rest uses. Same
        // defaults the renderer applies: unset src means
        // the whole buffer, unset dst means src size.
        let vp = *data.vp.lock().unwrap();
        let buf_dims = data
            .shown_buf
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.data::<super::BufInfo>())
            .map(|bi| (bi.w, bi.h));
        let raw_buf_dmg: Vec<(i32, i32, i32, i32)> =
            std::mem::take(&mut *data.buf_damage.lock().unwrap());
        // Upload clip for the per-SURFACE texture — the
        // texture always mirrors the last shown frame,
        // and per spec BOTH damage kinds describe the
        // diff against exactly that frame. So damage on
        // any commit — attach or damage-only — may patch
        // it; the only blockers are damage we can't map
        // into buffer space (surface-space rects under an
        // active viewport) and the spec's "attach with no
        // damage = whole surface changed" default.
        let vp_active = vp.src.is_some() || vp.dst.is_some();
        let cur_buf = data.shown_buf.lock().unwrap().as_ref().map(|b| b.id());
        let this_dmg: Option<Vec<(i32, i32, i32, i32)>> = if vp_active && !surf_dmg.is_empty() {
            None
        } else {
            let mut all = surf_dmg;
            all.extend_from_slice(&raw_buf_dmg);
            if new_attach && all.is_empty() {
                None
            } else {
                Some(all)
            }
        };
        {
            let mut u = data.upload_damage.lock().unwrap();
            // Accumulate over commits that outpace frames:
            // a patch of only the latest rects leaves
            // stale pixels behind. A pending full upload
            // (u.2 == None) stays full — the renderer marks
            // the state synced (Some(vec![])) once the
            // texture provably holds the latest content.
            match (u.2.is_some(), this_dmg) {
                (true, Some(new)) => {
                    let acc = u.2.as_mut().unwrap();
                    acc.extend_from_slice(&new);
                    // Unbounded rect lists cost more than
                    // a full upload — collapse instead.
                    if acc.len() > 24 {
                        u.2 = None;
                    }
                }
                (true, None) => u.2 = None,
                (false, _) => {}
            }
            u.0 = data.commit_gen.load(std::sync::atomic::Ordering::SeqCst);
            u.1 = cur_buf;
        }
        for (x, y, w, h) in raw_buf_dmg {
            let r = match buf_dims {
                Some((bw, bh)) if vp.src.is_some() || vp.dst.is_some() => {
                    let (sx, sy, sw, sh) = vp.src.unwrap_or((0.0, 0.0, bw as f64, bh as f64));
                    let (dw, dh) = vp.dst.unwrap_or((sw.ceil() as i32, sh.ceil() as i32));
                    let fx = dw as f64 / sw;
                    let fy = dh as f64 / sh;
                    (
                        ((x as f64 - sx) * fx) as i32,
                        ((y as f64 - sy) * fy) as i32,
                        (w as f64 * fx).ceil() as i32,
                        (h as f64 * fy).ceil() as i32,
                    )
                }
                _ => (x, y, w, h),
            };
            dmg.push(r);
        }
        if dmg.is_empty() && attached {
            // Whole-surface damage in surface-local space —
            // the viewport's effective size, not raw buffer
            // dims.
            let (w, h) = state.surf_size(data);
            dmg.push((0, 0, w, h));
        }
        // Damage translates through the SAME origin the
        // renderer uses — popups derive theirs from the
        // parent's position, toplevels from their own.
        let (dx, dy) = state.local_origin(surf);
        let (sx, sy) = *data.offset.lock().unwrap();
        for (x, y, w, h) in dmg {
            state
                .damage
                .push((dx as i32 + sx + x, dy as i32 + sy + y, w, h));
        }
        let shown = data.shown_buf.lock().unwrap().is_some();
        let cursor = data.is_cursor.load(std::sync::atomic::Ordering::SeqCst);
        let is_popup = state.popups.iter().any(|p| p.surf.id() == surf.id());
        if shown && state.layer_index(surf).is_some() {
            // Layer surfaces map through their own
            // configure/ack handshake — never through the
            // toplevel cascade.
            state.commit_layer(surf);
        } else if shown && state.xwin_or(surf) {
            // X11 override-redirect: floats at its X11
            // geometry, never joins order or tiling.
            state.map_xpopup(surf);
        } else if shown && !cursor && !is_popup && Self::has_window_role(data) {
            state.map_window(surf);
        } else if shown {
            // Aux surface (cursor image, drag icon, …) —
            // has content but no window role; it never
            // joins the order, hit-testing or focus.
            data.mapped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // The surface's own subsurface state (set_position /
        // place_*) is double-buffered — applies with the commit.
        state.sub_apply_state(surf);
        // A sync'd child's cached commit releases when its
        // parent applies one — cascade down the whole subtree.
        state.sub_flush_children(surf);
    }
}
