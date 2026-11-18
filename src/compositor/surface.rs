// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wl_surface — per-surface state and the commit path.
//!
//! SurfaceData rides as the wl_surface's user data (Mutex-wrapped
//! because `U: Send + Sync` is required; the compositor is
//! single-threaded so contention is zero by construction).
//! Commit moves pending→shown, handles map/unmap, and drives the
//! layout hook.

use tracing::info;
use wayland_protocols::xdg::shell::server::{xdg_surface, xdg_toplevel};
use wayland_server::protocol::{wl_buffer, wl_callback, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{BufInfo, CompState};

/// One entry in a surface's subsurface stack — either the
/// marker slot where the parent's own buffer composites, or a
/// child subsurface. Ordered bottom→top.
#[derive(Clone)]
pub enum SubItem {
    Root,
    Child(wl_surface::WlSurface),
}

/// Per-surface state attached as the wl_surface's user data.
#[derive(Default)]
pub struct SurfaceData {
    /// Tri-state pending buffer: outer Some = an attach ran since
    /// the last commit, inner Option = the buffer (None = the
    /// client attached NULL, hiding the surface). A commit
    /// WITHOUT a pending attach must keep the shown buffer —
    /// pure damage commits are legal and common (swaybg).
    pub pending_buf: std::sync::Mutex<Option<Option<wl_buffer::WlBuffer>>>,
    pub shown_buf: std::sync::Mutex<Option<wl_buffer::WlBuffer>>,
    /// Whether this surface has committed a buffer at least once.
    pub mapped: std::sync::atomic::AtomicBool,
    /// Frame callbacks the client is waiting on — `done` fires
    /// after the NEXT present, never inside the request handler
    /// (sending the destructor event mid-dispatch UAFs the
    /// dispatcher — that was the compositor crash).
    pub frames: std::sync::Mutex<Vec<wl_callback::WlCallback>>,
    /// Position in compositor space (move grab updates it).
    pub pos: std::sync::Mutex<(i32, i32)>,
    /// The window currently sits in a tile slot of a Tiling
    /// output — set by apply_layout's tile assign, cleared by
    /// float_restore. Position decides membership, never a
    /// global mode.
    pub tiled: std::sync::atomic::AtomicBool,
    /// Server-side decoration negotiated or assigned — the
    /// compositor draws the frame (titlebar, border, grips,
    /// close button) and the margins are ours, so the client
    /// renders content only.
    pub ssd: std::sync::atomic::AtomicBool,
    /// xdg_toplevel_icon — double-buffered like every xdg
    /// surface property: set_icon snapshots into pending, the
    /// next wl_surface.commit applies it to `icon`. Inner None
    /// resets to the default app icon.
    pub icon_pending: std::sync::Mutex<Option<Option<super::xdg_icon::IconData>>>,
    /// The live toplevel icon — taskbars/switchers read this.
    pub icon: std::sync::Mutex<super::xdg_icon::IconData>,
    /// Floating position saved when tiling takes over — restored
    /// on the switch back so a user's placement survives flips.
    pub float_pos: std::sync::Mutex<Option<(i32, i32)>>,
    /// xdg window-geometry offset — CSD clients pad their buffer
    /// with shadow margins (32px each side is a typical value). The
    /// tile places the geometry, so the buffer draws at
    /// pos - win_geom.xy for placement; win_geom.wh is the
    /// interactive extent for hit-testing (0,0 = unset → the
    /// whole surface rect counts).
    pub win_geom: std::sync::Mutex<(i32, i32, i32, i32)>,
    /// wl_surface.offset pending → applied delta. v5+ clients use
    /// this instead of negative attach offsets (GTK popups do).
    pub offset: std::sync::Mutex<(i32, i32)>,
    /// wl_pointer cursor role — drawn at the pointer position, never
    /// enters the stacking order or the focus hit-test.
    pub is_cursor: std::sync::atomic::AtomicBool,
    /// The surface's xdg role objects — needed to send size
    /// configures when the layout policy retiles.
    pub xdg_tl: std::sync::Mutex<Option<xdg_toplevel::XdgToplevel>>,
    pub xdg_surf: std::sync::Mutex<Option<xdg_surface::XdgSurface>>,
    /// Client-set title (xdg_toplevel.set_title) — shown later in
    /// window switchers/decorations; kept for focus logging today.
    pub title: std::sync::Mutex<String>,
    /// Client-set app_id (xdg_toplevel.set_app_id) — the desktop
    /// file stem taskbars and foreign-toplevel clients show.
    pub app_id: std::sync::Mutex<String>,
    /// Workspace membership — the window shows on whichever
    /// output currently displays this workspace (0-based).
    pub ws: std::sync::Mutex<u8>,
    /// Client-reported damage rects (surface-local) — drained
    /// into the compositor's frame damage on commit.
    pub damage: std::sync::Mutex<Vec<(i32, i32, i32, i32)>>,
    /// Pending wp_presentation_feedback objects — one-shot,
    /// fired at the present that next shows this surface.
    pub present_fbs: std::sync::Mutex<
        Vec<wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::WpPresentationFeedback>,
    >,
    /// Iconified by foreign-toplevel request — stays alive and
    /// in `order` but leaves rendering and hit-testing.
    pub minimized: std::sync::atomic::AtomicBool,
    /// xdg_wm_base v5+ — wm_capabilities goes out exactly once,
    /// before the first xdg_surface.configure (spec mandate).
    pub wm_caps_sent: std::sync::atomic::AtomicBool,
    /// wp_linux_drm_syncobj surface object — its presence puts
    /// this surface on the explicit-sync commit path.
    pub syncobj_surf: std::sync::Mutex<
        Option<
            wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
        >,
    >,
    /// Double-buffered acquire/release points from
    /// set_acquire_point/set_release_point — consumed by the
    /// next commit, like the attach itself.
    pub sync_acq: std::sync::Mutex<Option<crate::compositor::syncobj::SyncPoint>>,
    pub sync_rel: std::sync::Mutex<Option<crate::compositor::syncobj::SyncPoint>>,
    /// A committed attach held back until its fence signals —
    /// explicit (syncobj timeline) or implicit (dmabuf fences).
    pub sync_stash: std::sync::Mutex<Option<crate::compositor::syncobj::SyncStash>>,
    /// Release point belonging to the currently shown buffer —
    /// queued for deferred signaling when it leaves the screen.
    pub release_of: std::sync::Mutex<Option<crate::compositor::syncobj::SyncPoint>>,
    /// ext_session_lock surface — while a lock is active only
    /// these surfaces render and accept input.
    pub is_lock_surface: std::sync::atomic::AtomicBool,
    /// Current toplevel states we last configured — drives
    /// activated-on-focus visuals on the client side.
    pub tl_states: std::sync::Mutex<Vec<xdg_toplevel::State>>,
    /// The size we last configured — kept so state-only changes
    /// (activated on focus) don't un-pin a tile size.
    pub cfg_size: std::sync::Mutex<(i32, i32)>,
    /// X window id when this surface belongs to Xwayland — set by
    /// the XWM on WL_SURFACE_ID. X surfaces position via their
    /// X11 geometry, never via the toplevel cascade.
    pub xwin: std::sync::Mutex<Option<u32>>,
    /// wp_viewport pending crop+scale — applied to `vp` on commit,
    /// same double-buffering as the buffer attach itself.
    pub vp_pending: std::sync::Mutex<super::viewporter::Viewport>,
    /// Committed viewport state — renderer and hit-test read this.
    pub vp: std::sync::Mutex<super::viewporter::Viewport>,
    /// wp_alpha_modifier pending multiplier — applies at commit;
    /// None means no set_multiplier since the last commit.
    pub alpha_pending: std::sync::Mutex<Option<f64>>,
    /// Committed alpha multiplier — None means identity (1.0).
    pub alpha: std::sync::Mutex<Option<f64>>,
    /// Guards the one-alpha-object-per-surface rule.
    pub alpha_obj: std::sync::Mutex<bool>,
    /// wp_content_type pending hint — applies at commit.
    pub ctype_pending: std::sync::Mutex<Option<u32>>,
    /// Committed content hint — wp_content_type_v1 enum value.
    pub content_type: std::sync::Mutex<u32>,
    /// Guards the one-content-type-object-per-surface rule.
    pub ctype_obj: std::sync::Mutex<bool>,
    /// xdg_dialog_v1 modal flag — focus policy redirects
    /// ancestors through `modal_descendant` while set.
    pub modal: std::sync::Mutex<bool>,
    /// wp_tearing_control pending hint — applies at commit.
    pub tearing_pending: std::sync::Mutex<Option<bool>>,
    /// Committed async-presentation flag — `tearing_active`
    /// scans this across `order` for the page-flip flags.
    pub tearing: std::sync::Mutex<bool>,
    /// Guards the one-tearing-object-per-surface rule.
    pub tear_obj: std::sync::Mutex<bool>,
    /// damage_buffer rects (buffer space) — kept separate from
    /// `damage` (surface space) because a viewport maps the two
    /// coordinate spaces differently.
    pub buf_damage: std::sync::Mutex<Vec<(i32, i32, i32, i32)>>,
    /// wp_fractional_scale_v1 addon object — the compositor's
    /// scale announcement channel for this surface.
    pub fs_obj: std::sync::Mutex<Option<super::fractional_scale::WpFractionalScaleV1>>,
    /// Last preferred_scale announced (×120) — refresh() only
    /// re-sends on change.
    pub fs_scale: std::sync::Mutex<u32>,
    /// zwp_keyboard_shortcuts_inhibitor_v1 — while Some+alive and
    /// the surface holds keyboard focus, compositor bindings are
    /// suspended and every key forwards to this client.
    pub kbd_inhibitor:
        std::sync::Mutex<Option<super::kbd_inhibit::ZwpKeyboardShortcutsInhibitorV1>>,
    /// Transient parent — set via xdg_toplevel.set_parent (same
    /// client) or xdg_foreign set_parent_of (cross-client). The
    /// stacking order keeps children directly above it.
    pub transient_for: std::sync::Mutex<Option<wl_surface::WlSurface>>,
    /// Committed content version — bumped on every commit so GPU
    /// texture caches re-upload buffers the client rewrote under
    /// the same wl_buffer id (in-place damage commits).
    pub commit_gen: std::sync::atomic::AtomicU64,
    /// Buffer-space damage paired with the generation and buffer
    /// it belongs to — `(commit_gen, buf_id, rects)` under one
    /// lock so a renderer can never pair a torn read. Rects
    /// accumulate across commits that land between frames (a
    /// missed commit must not lose its damage — that produced
    /// half-stale patched textures = flicker), but only while the
    /// shown buffer id stays the same: damage_buffer is relative
    /// to the buffer's own previous content, so an attach of a
    /// different buffer resets the accumulation. `None` rects
    /// force a full upload (undamaged attach, viewport active,
    /// or rect overflow); `Some` patches only changed rows.
    pub upload_damage: std::sync::Mutex<(
        u64,
        Option<wayland_server::backend::ObjectId>,
        Option<Vec<(i32, i32, i32, i32)>>,
    )>,
    /// wl_surface.set_input_region — the surface-local rect
    /// union that receives pointer input. `None` = the whole
    /// surface (protocol default); `Some(vec![])` = explicitly
    /// input-transparent. Honored in hit-testing — XWayland
    /// translates XShape input masks through this, so ignoring
    /// it lets invisible overlay windows eat clicks.
    pub input_region: std::sync::Mutex<Option<Vec<(i32, i32, i32, i32)>>>,
    /// xdg_toplevel.set_min_size / set_max_size — the client's
    /// own resize bounds in window-geometry space. The
    /// interactive resize grab clamps to them; (0,0) = unset.
    pub min_size: std::sync::Mutex<(i32, i32)>,
    pub max_size: std::sync::Mutex<(i32, i32)>,
    /// wl_subcompositor role: the parent surface this surface is
    /// attached to. Subsurfaces never enter `order` — they draw
    /// at parent_origin + sub_pos, interleaved with the parent's
    /// buffer by the parent's sub_stack ordering.
    pub sub_parent: std::sync::Mutex<Option<wl_surface::WlSurface>>,
    /// Ordered child list INCLUDING the SubItem::Root marker for
    /// the parent's own buffer — place_above/place_below move
    /// entries around it, which is what makes children draw
    /// above or below their parent's pixels.
    pub sub_stack: std::sync::Mutex<Vec<SubItem>>,
    /// Applied position in parent-surface-local space —
    /// double-buffered: set_position lands in sub_pending_pos
    /// and applies at commit (or the parent's, when sync'd).
    pub sub_pos: std::sync::Mutex<(i32, i32)>,
    pub sub_pending_pos: std::sync::Mutex<Option<(i32, i32)>>,
    /// Pending restack — (sibling, above): applied to the
    /// parent's sub_stack at commit like the position.
    pub sub_pending_place:
        std::sync::Mutex<Option<(wayland_server::backend::ObjectId, bool)>>,
    /// Sync mode — the SPEC DEFAULT is synchronized (a subsurface
    /// starts sync'd, so Default::default must stay false and
    /// get_subsurface sets it true explicitly): commits cache
    /// until the parent applies a commit. Desync commits apply
    /// immediately.
    pub sub_sync: std::sync::atomic::AtomicBool,
    /// A commit is cached waiting for the parent's next commit —
    /// the sync-mode holding state.
    pub sub_cached: std::sync::atomic::AtomicBool,
}

impl CompState {
    /// A surface may join the stacking order only with a real
    /// window role — an xdg toplevel or an Xwayland-associated
    /// X window. Bare wl_surfaces that commit buffers are aux
    /// surfaces (pointer cursor, drag icon, subwindows): pushing
    /// them into `order` gave them tiles and, worse, focus-on-map
    /// sent them wl_keyboard.enter — clients resolve a surface's
    /// enter target to their window and segfault on NULL for aux
    /// surfaces (real clients have died exactly like this).
    pub(crate) fn has_window_role(sd: &SurfaceData) -> bool {
        sd.xdg_tl.lock().unwrap().is_some() || sd.xwin.lock().unwrap().is_some()
    }

    /// Push a role-ful surface into the stacking order: cascade
    /// placement (or its X11 geometry for X windows), retile and
    /// focus-on-map. Called from the first buffer commit — and
    /// from `try_bind` when an X surface committed its buffer
    /// before the WL_SURFACE_ID association arrived.
    pub(crate) fn map_window(&mut self, surf: &wl_surface::WlSurface) {
        if self.order.iter().any(|s| s.id() == surf.id()) {
            return;
        }
        let Some(data) = surf.data::<SurfaceData>() else {
            return;
        };
        data.mapped.store(true, std::sync::atomic::Ordering::SeqCst);
        // New toplevels open centered on the primary output,
        // cascaded so siblings don't stack pixel-exact on top of
        // each other.
        {
            let mut pos = data.pos.lock().unwrap();
            if *pos == (0, 0) {
                // X windows take their X11 geometry — the cascade
                // below is for native toplevels only.
                let xpos = data
                    .xwin
                    .lock()
                    .unwrap()
                    .and_then(|wid| self.xwins.get(&wid).map(|x| (x.x, x.y)));
                if let Some(p) = xpos {
                    *pos = p;
                } else {
                    // Cascade inside the usable rect — a top-edge
                    // panel's exclusive zone keeps new windows out
                    // from under it.
                    let (rx, ry, rw, rh) = self.usable_rect();
                    let (bw, bh) = data
                        .shown_buf
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|b| b.data::<BufInfo>())
                        .map(|bi| (bi.w, bi.h))
                        .unwrap_or((0, 0));
                    let cascade = (self.order.len() as i32 % 8) * 32;
                    *pos = (
                        rx + (rw as i32 - bw) / 2 + cascade,
                        ry + (rh as i32 - bh) / 2 + cascade,
                    );
                }
            }
        }
        self.ws_assign_new(&surf);
        self.order.push(surf.clone());
        // A newly mapped transient rides directly above its
        // parent — restack settles chains in one pass.
        self.restack_transients();
        // Taskbars/docks learn about the new window before the
        // first focus event so the handle exists when Activated
        // state lands.
        self.ft_new(surf);
        info!("surface mapped — joins compositing order");
        // Under tiling a new window slots into the arrangement
        // immediately instead of landing centered like a floater.
        self.apply_layout();
        // Focus follows map — clients that need a serial
        // (clipboard tools, dialogs) wait on the keyboard enter
        // forever otherwise.
        self.set_kbd_focus(Some(surf.clone()));
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_callback::WlCallback,
        _: wl_callback::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
