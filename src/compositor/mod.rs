// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Compositor core — the real wayland server inside odin.
//!
//! Globals: wl_compositor, wl_shm, xdg_wm_base, wl_seat, wl_output,
//! wl_data_device_manager. Clients connect on our socket, get
//! toplevel windows, and their shm buffer contents are composited
//! into the frame the active backend presents. This module is
//! backend-agnostic — nested and udev both drive it.
//!
//! Layout: one file per protocol family, plus this root module
//! carrying CompState, the global bind impls and helpers every
//! family shares (serials, client matching, focus helpers).

pub mod xpopup;
pub mod xwin;
pub use xwin::Xwin;
pub mod activation;
pub mod alpha;
pub mod bell;
mod blit;
mod composite;
pub mod content_type;
pub mod copy_capture;
pub mod copy_frame;
mod cursor_shape;
pub mod damage;
pub mod data;
pub mod data_control;
pub mod decoration;
pub mod dialog;
pub mod dmabuf;
pub mod dnd;
pub mod ext_data_control;
pub mod ext_ftl;
pub mod foreign;
pub mod foreign_toplevel;
pub mod fractional_scale;
pub mod gamma;
pub mod gestures;
mod globals;
pub mod idle_inhibit;
pub mod idle_notify;
pub mod input_inhibit;
pub mod input_method;
pub mod kbd_inhibit;
pub mod key_event;
pub mod keymap;
pub mod layer;
mod layer_dispatch;
pub mod outpower;
pub mod output_config;
pub mod output_mgmt;
pub mod pointer;
pub mod pointer_ext;
pub mod pointer_grab;
mod pointer_hit;
pub mod present;
pub mod primary_sel;
pub mod prune;
pub mod pump;
pub mod render;
mod scanout;
pub mod screencopy;
pub mod seat;
pub mod session_lock;
pub mod shm;
pub mod spb;
pub(crate) mod ssd;
pub mod subsurface;
pub mod surface;
pub mod surface_dispatch;
pub mod sync_wait;
pub mod syncobj;
pub mod tablet;
pub mod tablet_events;
pub mod tearing;
pub mod text_input;
pub mod touch;
pub mod viewporter;
pub mod virtual_input;
pub mod vkeyboard;
pub mod win;
pub mod workspace;
pub mod ws_ext;
mod ws_ext_dispatch;
pub mod xdg;
mod xdg_dispatch;
pub mod xdg_icon;
pub mod xdg_output;

use std::time::Instant;
use wayland_server::protocol::{
    wl_buffer, wl_data_device, wl_keyboard, wl_output, wl_pointer, wl_surface, wl_touch,
};
use wayland_server::{Client, Display, DisplayHandle, Resource};

pub use pump::pump;
pub use seat::{Grab, GrabKind};
pub use shm::BufInfo;
pub use surface::{SubItem, SurfaceData};

/// Millisecond timestamp base — Wayland event times are u32 ms
/// since an arbitrary epoch; boot-relative is fine.
pub fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

/// Compositor state: server + toplevel stacking order.
/// Per-client identity — the peer pid/comm captured at accept
/// time so connect AND disconnect log lines can name the client;
/// a silent client is otherwise indistinguishable in the log.
pub struct CData {
    pub pid: i32,
    pub comm: String,
}
impl CData {
    /// Identity for clients we spawn ourselves (Xwayland) where
    /// peer credentials describe our own process.
    pub fn spawn() -> Self {
        CData {
            pid: std::process::id() as i32,
            comm: "odin-spawn".into(),
        }
    }
}
impl wayland_backend::server::ClientData for CData {
    fn initialized(&self, _: wayland_backend::server::ClientId) {}
    fn disconnected(
        &self,
        _: wayland_backend::server::ClientId,
        reason: wayland_backend::server::DisconnectReason,
    ) {
        tracing::info!(pid = self.pid, comm = %self.comm, ?reason, "wayland client disconnected");
    }
}

pub struct CompState {
    pub display: Option<Display<CompState>>,
    /// DisplayHandle clone — stays valid while pump() has the
    /// display taken out for dispatch_clients, so resource
    /// creation works from inside request handlers too.
    pub dh: DisplayHandle,
    /// Surfaces in stacking order (bottom→top).
    pub order: Vec<wl_surface::WlSurface>,
    /// xdg popups currently live — drawn above all toplevels,
    /// dismissed on outside click or when the parent unmaps.
    /// Stored as (popup surface, parent surface, xdg_popup obj).
    pub popups: Vec<Popup>,
    /// X11 override-redirect windows (menus, tooltips, dropdowns)
    /// — ICCOM says they bypass the WM entirely: no stacking
    /// order, no tiling, no map focus grab. They float at their
    /// X11 geometry above the window stack like xdg popups do.
    pub xpopups: Vec<wl_surface::WlSurface>,
    /// Every bound wl_pointer across clients — events fan out to
    /// the one whose surface holds focus.
    pub pointers: Vec<wl_pointer::WlPointer>,
    pub keyboards: Vec<wl_keyboard::WlKeyboard>,
    /// Wayland serial counter — real serials, never hardcoded.
    pub serial: u32,
    /// Pointer focus — the surface under the cursor (sloppy:
    /// follows motion). Drives enter/leave/motion/button events.
    /// Popups also take this — hit-test walks popups first.
    pub focus: Option<wl_surface::WlSurface>,
    /// Keyboard focus — sticky: changes only on a click or an
    /// explicit focus action, never on pointer motion.
    pub kbd_focus: Option<wl_surface::WlSurface>,
    /// Whether pointer focus was already entered.
    pub ptr_inside: bool,
    /// Cursor position in global space — fed by libinput deltas.
    pub ptr_pos: (f64, f64),
    /// Raw motion arrived since the last client dispatch — flush
    /// sends ONE motion with the latest position per frame. A
    /// 1kHz mouse would otherwise push ~1000 repaints/s into a
    /// client that can only draw ~60 (event backlog = the
    /// seconds-long lag after leaving a hover-repainting menu).
    pub motion_pending: bool,
    /// Accumulated unaccelerated+accelerated deltas for
    /// relative-pointer clients — grouped into one event per
    /// frame like the absolute path.
    pub rel_pending: (f64, f64, f64, f64),
    /// Buffers swapped out of `shown_buf` — released only after
    /// a frame that no longer samples them has been presented.
    /// With dmabuf textures the client may legally rewrite a
    /// released buffer at once, so releasing at attach time let
    /// clients paint over pixels the CRTC was still scanning
    /// (the sporadic import-path flicker). Two frame_done hops
    /// of margin: pending → next → released.
    pub pending_release: Vec<wl_buffer::WlBuffer>,
    /// One presented frame older than `pending_release`.
    pub release_next: Vec<wl_buffer::WlBuffer>,
    /// Two presented frames old — nothing still scans them.
    pub release_ready: Vec<wl_buffer::WlBuffer>,
    /// Client-provided cursor surface from wl_pointer.set_cursor.
    pub cursor_surf: Option<wl_surface::WlSurface>,
    /// Cursor hotspot — offset inside the cursor image.
    pub cursor_hotspot: (i32, i32),
    /// Themed cursor picked via wp_cursor_shape — Some wins over
    /// cursor_surf (last request wins), cleared by set_cursor.
    pub cursor_img: Option<crate::xcursor::CursorImg>,
    /// Cursor image generation — bumped on every image/hotspot
    /// change so hardware cursor planes re-upload their BO.
    pub cursor_gen: u64,
    /// Connector names whose pointer lives on a hardware cursor
    /// plane — scene composites skip drawing it there entirely.
    pub hw_cursor: std::collections::HashSet<String>,
    /// Any output lacks a cursor plane — the composite must keep
    /// drawing the pointer and motion must keep damaging its
    /// pixels. Probed per output by the DRM backend.
    pub sw_cursor: bool,
    /// zwp_pointer_gestures objects — events fan out to the
    /// pointer-focus client's objects only.
    pub swipe_gestures: Vec<gestures::ZwpPointerGestureSwipeV1>,
    pub pinch_gestures: Vec<gestures::ZwpPointerGesturePinchV1>,
    pub hold_gestures: Vec<gestures::ZwpPointerGestureHoldV1>,
    /// zwp_text_input_v3 objects — focus sweep sends enter/leave
    /// and feeds enabled ones to the input method.
    pub text_inputs: Vec<text_input::ZwpTextInputV3>,
    /// zwp_input_method_v2 — at most one live IM per seat;
    /// a new get_input_method evicts the old.
    pub im: Option<input_method::ZwpInputMethodV2>,
    /// IM candidate popups — drawn above the focused window,
    /// anchored at the field's cursor rect.
    pub im_popups: Vec<ImPopup>,
    /// Bound wl_touch objects — events route per owning client.
    pub touch_objs: Vec<wl_touch::WlTouch>,
    /// Live touch points: slot → grabbed surface + local offset.
    pub touch_pts: std::collections::BTreeMap<i32, touch::TouchPoint>,
    /// Touch objects that received events since the last frame —
    /// frame() fans out only to these (protocol ids).
    pub touch_dirty: std::collections::HashSet<u32>,
    /// Down-serial → touch slot: xdg_toplevel.move/resize quotes
    /// the serial of the initiating event — a touch serial means
    /// the grab releases on finger lift, not a pointer button.
    pub touch_serials: std::collections::BTreeMap<u32, i32>,
    /// xdg-decoration objects — enforces one-decoration-
    /// per-toplevel; the object itself is client-lived.
    pub deco_objs: Vec<decoration::ZxdgToplevelDecorationV1>,
    /// xdg_foreign exports: handle token → exported surface.
    pub exported: std::collections::BTreeMap<String, wl_surface::WlSurface>,
    /// Live imported objects — need `destroyed` when their
    /// exported parent surface dies.
    pub imported: Vec<foreign::ZxdgImportedV2>,
    /// v1 import objects — same ImportedData, older protocol
    /// interface; swept alongside v2 in foreign_prune.
    pub imported_v1: Vec<foreign::ZxdgImportedV1>,
    /// Resolved themed cursor images, keyed by (name, size) —
    /// SetShape fires per hover region, so theme files must not
    /// hit the filesystem twice.
    pub cursor_cache: std::collections::HashMap<(String, u32), Option<crate::xcursor::CursorImg>>,
    /// Relative-pointer objects — raw deltas fan out to the
    /// pointer-focused client's objects each motion event.
    pub rel_ptrs: Vec<pointer_ext::ZwpRelativePointerV1>,
    /// At most one live pointer constraint (lock or confine).
    pub constraint: Option<pointer_ext::Constraint>,
    /// Live layer surfaces — panels, wallpapers, launchers. They
    /// never join `order`; draw/hit order is background, bottom,
    /// toplevels, top, overlay.
    pub layers: Vec<layer::Layer>,
    /// Global-space bounding box the pointer may reach.
    pub space: (f64, f64),
    /// Every bound wl_output with the connector it describes —
    /// one global per layout output, so a client sees each
    /// physical monitor as its own object.
    pub outputs: Vec<(wl_output::WlOutput, String)>,
    /// foreign-toplevel clients (taskbars/docks) + the live
    /// handle objects they hold per toplevel.
    pub ft_managers: Vec<foreign_toplevel::ZwlrForeignToplevelManager>,
    pub ft_handles: Vec<foreign_toplevel::FtLink>,
    /// Output-management: bound manager clients, their head
    /// objects, and the configuration serial every done event
    /// and create_configuration transaction anchors on.
    pub om_managers: Vec<output_mgmt::ZwlrOutputManager>,
    pub om_heads: Vec<output_mgmt::OmHead>,
    pub om_serial: u32,
    /// Connector name → wl_output GlobalId — kept so a hot-unplug
    /// can withdraw exactly that connector's global.
    pub out_globals: std::collections::HashMap<String, wayland_server::backend::GlobalId>,
    /// Applied output-configuration changes queued for the
    /// backend: the protocol commit validates + records them
    /// here, the event loop executes them against the live DRM
    /// outputs (real re-modeset, viewport move, CRTC blank).
    pub pending_outops: Vec<output_mgmt::OutOp>,
    /// Per-output gamma ramps (night light) — connector name →
    /// (owning control object, 3×256 u16 table). Applied at
    /// present, never to capture; dies with its control object.
    pub gamma: std::collections::BTreeMap<
        String,
        (
            wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1,
            std::sync::Arc<Vec<u16>>,
        ),
    >,
    /// zwp_tablet_manager_v2 — bound seats plus every
    /// discovered tablet/tool/pad, tracked per seat object.
    pub tablet_seats: Vec<tablet::ZwpTabletSeat>,
    pub tablets: Vec<tablet::TabletDev>,
    pub tools: Vec<tablet::ToolState>,
    pub pads: Vec<tablet::PadDev>,
    /// Per-output power state (zwlr_output_power_manager_v1) —
    /// connector name → powered. Missing entries mean on; the
    /// backends read `powered()` every frame and blank/resume.
    pub power: std::collections::BTreeMap<String, bool>,
    /// The one active input inhibitor (screen locker) — while
    /// Some, only that client's surfaces see input.
    pub inhibitor: Option<wayland_protocols_wlr::input_inhibitor::v1::server::zwlr_input_inhibitor_v1::ZwlrInputInhibitorV1>,
    /// Global-space damage rects since the last present —
    /// surface commits translate their client damage in here.
    pub damage: Vec<(i32, i32, i32, i32)>,
    /// Structural scene change (map/move/retile/ws/minimize)
    /// — the next frame composites whole, not clipped.
    pub damage_full: bool,
    /// Latch — damage drains once per frame sweep; all outputs
    /// of that sweep share `frame_damage`.
    pub damage_taken: bool,
    /// The drained damage of the current sweep (see latch).
    pub frame_damage: Option<Vec<(i32, i32, i32, i32)>>,
    /// Presentation sequence — one number per presented frame,
    /// reported through wp_presentation_feedback.presented.
    pub present_seq: u64,
    /// Frame accounting for render-path diagnostics — counted
    /// per present sweep and logged periodically at debug level
    /// so a full-damage storm shows up in the log instead of
    /// being felt as "the mouse is slow".
    pub stat_frames: u64,
    pub stat_full: u64,
    pub stat_last: Instant,
    /// Slowest render/sweep block seen since the last stats log
    /// — surfaces loop starvation caused by expensive frames.
    pub stat_frame_ms: f64,
    /// Oldest libinput event latency (kernel timestamp → dispatch)
    /// seen since the last stats log — the "mouse feels slow"
    /// number directly.
    pub stat_input_ms: f64,
    /// Slowest backend render() call in the window — GPU upload/
    /// draw/readback or CPU composite time.
    pub stat_render_ms: f64,
    /// Slowest present() call in the window — dumb-buffer copy,
    /// dirtyfb, page-flip ioctl time.
    pub stat_present_ms: f64,
    /// Slowest scanout memcpy inside present() — the copy alone,
    /// separated from the ioctls that follow it.
    pub stat_copy_ms: f64,
    /// Slowest dirty_framebuffer ioctl inside present() — the
    /// virtio guest→host flush, clip-scaled.
    pub stat_dirty_ms: f64,
    /// Slowest dirtyfb + page_flip ioctl block inside present().
    pub stat_flip_ms: f64,
    /// Slowest GPU fence wait inside the vulkan render() — the
    /// previous submission's draw+drain still running on-device.
    pub stat_wait_ms: f64,
    /// Slowest CPU upload block inside render() — client surface
    /// memcpys into host-linear textures.
    pub stat_upload_ms: f64,
    /// Slowest render-pass recording block inside render() —
    /// command recording, normally sub-millisecond; a spike here
    /// points at driver-internal work inside recording calls.
    pub stat_draw_ms: f64,
    /// Slowest queue_submit call — on some drivers the first
    /// submit referencing freshly bound memory materializes the
    /// physical backing, which stalls the caller outright.
    pub stat_submit_ms: f64,
    /// Slowest wayland client dispatch (pump) in the window —
    /// a spike here means client request processing itself is
    /// starving the loop, e.g. a client flooding its socket.
    pub stat_pump_ms: f64,
    /// Slowest X11 wm event drain + compositor→X request apply
    /// in the window — X traffic storms land here.
    pub stat_xwm_ms: f64,
    /// Slowest libinput drain itself (not the queue lag) —
    /// separates "input callback is expensive" from "the loop
    /// never got to input".
    pub stat_drain_ms: f64,
    /// Biggest single-frame upload volume in KiB — separates
    /// "many small patches" from "one huge re-upload", which
    /// look identical inside `stat_upload_ms`.
    pub stat_up_kb: u64,
    /// Biggest single-frame upload count (full + patch) — a
    /// sustained high count means clients re-commit faster than
    /// the cache can hold, or offscreen surfaces still upload.
    pub stat_up_texs: u64,
    /// Largest single surface upload seen in the window (KiB)
    /// with its dimensions — names the dominant producer when
    /// `stat_up_kb` runs hot.
    pub stat_up_big_kb: u64,
    /// Width of the biggest upload.
    pub stat_up_big_w: i32,
    /// Height of the biggest upload.
    pub stat_up_big_h: i32,
    /// (surface, inhibitor) pairs — a surface vetoes idle while
    /// its inhibitor lives; queried by the future blanking path.
    pub idle_inhibitors: Vec<idle_inhibit::Inhibitor>,
    /// Pending axis source announced by a virtual pointer's
    /// axis_source request — applies to the next axis event.
    pub vp_axis_source: u32,
    /// Keybindings loaded at startup.
    pub cfg: crate::config::Config,
    /// Live modifier mask on real input (udev) — tracked from
    /// libinput key events (ctrl=1, alt=2, shift=4, super=8) AND
    /// mirrored into wl_keyboard.modifiers for clients.
    pub udev_mods: u32,
    /// Active pointer grab (window move/resize) — None normally.
    pub grab: Option<Grab>,
    /// Coalesced resize configure for a grabbed NATIVE toplevel —
    /// raw 1kHz motion would otherwise fire one xdg configure per
    /// event and flood the client; the newest target size is
    /// applied once per frame tick in flush_motion.
    pub rs_pending: Option<(wl_surface::WlSurface, i32, i32)>,
    /// Key repeat state — armed while a repeatable key is held;
    /// the backend's frame tick calls repeat_tick().
    pub repeat_key: Option<u32>,
    pub repeat_next: Option<Instant>,
    /// Every bound wl_data_device — selection pushes fan out to
    /// the device(s) of the keyboard-focused client.
    pub data_devices: Vec<wl_data_device::WlDataDevice>,
    /// Currently advertised clipboard — set via wl_data_device
    /// or zwlr_data_control; SelSrc covers both owner kinds.
    pub selection: Option<data::SelSrc>,
    /// An in-flight drag — implicit pointer grab routing
    /// drag events instead of pointer events while alive.
    pub drag: Option<dnd::Drag>,
    /// Held pointer buttons — start_drag requires one down.
    pub ptr_down: u32,
    /// Bound data-control devices — clipboard managers watch
    /// every selection change through these.
    pub ctrl_devices: Vec<wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    /// ext_data_control devices — the stabilized sibling of
    /// ctrl_devices; both channels observe the same selection.
    pub ext_ctrl_devices: Vec<wayland_protocols::ext::data_control::v1::server::ext_data_control_device_v1::ExtDataControlDeviceV1>,
    /// ext_foreign_toplevel_list lists — the read-only taskbar
    /// channel mirroring ft_managers/ft_handles.
    pub ext_ftl_lists:
        Vec<wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    /// Live ext toplevel handles, one per (list, window) pair.
    pub ext_ftl_handles: Vec<crate::compositor::ext_ftl::ExtFtLink>,
    /// Live ext_image_copy_capture sessions with their resolved
    /// source — output removal and toplevel close sweep this to
    /// send `stopped` instead of leaving dead sessions dangling.
    pub cap_sessions: Vec<(
        wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        crate::compositor::copy_capture::CapSrc,
    )>,
    /// Surfaces holding a stashed attach — the frame tick polls
    /// their fences (explicit syncobj point or implicit dmabuf
    /// fence) and lands the buffer once signaled.
    pub sync_waiters: Vec<wl_surface::WlSurface>,
    /// Deferred syncobj release-point signals — the same
    /// pending→next→ready staging wl_buffer.release uses:
    /// two presented frames before the client's timeline sees
    /// the point, so the GPU is provably done sampling.
    pub tsig_pending: Vec<crate::compositor::syncobj::SyncPoint>,
    pub tsig_next: Vec<crate::compositor::syncobj::SyncPoint>,
    pub tsig_ready: Vec<crate::compositor::syncobj::SyncPoint>,
    /// Primary selection owner — unified SelSrc so a
    /// data-control manager and a wp_primary_selection owner
    /// share the same slot.
    pub primary: Option<data::SelSrc>,
    /// Bound zwp_primary_selection devices — the focused
    /// client's devices get selection pushes.
    pub prim_devices: Vec<wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,
    /// Bound ext_workspace managers — each tracks its own
    /// group and workspace handles, the last-published state
    /// for diffing, and a pending queue for commit batches.
    pub ws_clients: Vec<ws_ext::WsClient>,
    /// Output layout + window arrangement policy.
    pub layout: crate::wm::Layout,
    /// Scene-changed flag — damage-driven frame pacing.
    pub dirty: bool,
    /// Frame-dump generation — the dump-frame binding bumps it;
    /// every output dumps exactly once per generation at its
    /// next present, so all monitors get a file, not just the
    /// first one to render.
    pub dump_seq: u64,
    /// quit binding fired — the backend loop exits on it so the
    /// process ends through the normal drop path (seat release,
    /// DRM cleanup, "exiting cleanly" log) instead of dying
    /// mid-dispatch via process::exit.
    pub quit: bool,
    /// Every wl_surface ever created — the XWM resolves
    /// Xwayland's WL_SURFACE_ID messages against this list
    /// (protocol ids are client-local; only Xwayland matches).
    pub all_surfaces: Vec<wl_surface::WlSurface>,
    /// X window id → state — owned by the compositor, mutated by
    /// the XWM event drain.
    pub xwins: std::collections::BTreeMap<u32, Xwin>,
    /// The X display number Xwayland got — exported to spawned
    /// clients as DISPLAY so X apps reach their server.
    pub x_display: Option<u32>,
    /// Wayland client handle of the Xwayland process — surface ids
    /// in WL_SURFACE_ID messages only exist in its namespace.
    pub xwl_client: Option<wayland_server::Client>,
    /// Outbound X requests queued by compositor actions (focus,
    /// moves, raises) — drained onto the X connection each tick
    /// so compositor code never touches x11rb directly.
    pub xreqs: Vec<crate::xwm::XReq>,
    /// Paste fetches queued by receive() on an X-owned selection —
    /// (X owner window, mime, paste fd); the XWM turns each into
    /// a SelectionRequest and pumps the reply into the fd.
    pub x_paste_q: Vec<(u32, String, std::os::fd::OwnedFd)>,
    /// Last real input event — the idle-notify clock's epoch.
    pub last_input: std::time::Instant,
    /// ext_idle_notifier_v1 watchers — scanned per frame tick.
    pub idle_notifs: Vec<idle_notify::IdleNotif>,
    /// ext_session_lock_v1 — its presence means the session is
    /// locked: only lock surfaces render and take input.
    pub lock: Option<session_lock::SessionLock>,
    /// Committed xdg-activation tokens awaiting an activate call.
    pub act_tokens: std::collections::HashMap<String, activation::ActToken>,
    /// dev_t of the primary DRM node — announced to clients via
    /// the dmabuf v4 `main_device` event so EGL stacks (XWayland
    /// glamor!) open the same GPU the compositor uses.
    /// 0 on backends without a DRM device.
    pub drm_devt: u64,
    /// Canonical DRM device fd for syncobj ioctls — timeline
    /// imports happen in THIS fd's handle table because drivers
    /// (NVIDIA) reject `fd_to_handle` on the client's exported
    /// syncobj fd. One shared file description also makes handle
    /// equality mean "same kernel syncobj" for conflicting-point
    /// checks. Opened lazily from the first render node.
    pub drm_fd: Option<std::os::fd::OwnedFd>,
}

/// A live xdg popup — its position derives from the parent's
/// position plus the positioner request, recomputed on map.
/// One input-method popup — surface plus its protocol object;
/// the object carries text_input_rectangle events to the client.
pub struct ImPopup {
    pub surf: wl_surface::WlSurface,
    pub obj: input_method::ZwpInputPopupSurfaceV2,
}

pub struct Popup {
    pub surf: wl_surface::WlSurface,
    pub parent: wl_surface::WlSurface,
    pub obj: wayland_protocols::xdg::shell::server::xdg_popup::XdgPopup,
    /// Positioner-result offset relative to the parent surface.
    pub rel: (i32, i32),
    pub size: (i32, i32),
}

impl CompState {
    fn next_serial(&mut self) -> u32 {
        let s = self.serial;
        self.serial = self.serial.wrapping_add(1);
        s
    }

    /// Serial allocator for the xdg submodule — same counter,
    /// crate-visible because next_serial stays private here.
    pub(crate) fn next_serial_pub(&mut self) -> u32 {
        self.next_serial()
    }

    /// Pointer/keyboard objects must only ever receive events whose
    /// object arguments belong to the SAME client.
    pub(crate) fn client_matches(
        &self,
        res: &wl_surface::WlSurface,
        obj_client: Option<Client>,
    ) -> bool {
        // An active input inhibitor (lock screen) owns input —
        // objects of every other client go dark until it dies.
        if !self.input_allowed(obj_client.as_ref()) {
            return false;
        }
        res.client().map(|c| c.id()) == obj_client.map(|c| c.id())
    }

    /// Push all buffered outbound events to client sockets.
    pub fn flush_clients(&mut self) {
        if let Some(d) = self.display.as_mut() {
            let _ = d.flush_clients();
        }
    }

    /// Fire `done` on every queued frame callback — called by the
    /// backend right after a frame is presented.
    /// Key-repeat pump — backends call this once per frame tick.
    /// Fires a synthetic press+release at the repeat rate while a
    /// repeatable key is held (400ms delay, 25Hz repeat).
    pub fn repeat_tick(&mut self) {
        let (Some(key), Some(next)) = (self.repeat_key, self.repeat_next) else {
            return;
        };
        if Instant::now() < next {
            return;
        }
        self.repeat_next = Some(Instant::now() + std::time::Duration::from_millis(40));
        self.send_key(key, true);
        self.send_key(key, false);
    }
}
