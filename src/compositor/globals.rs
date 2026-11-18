// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Global registration — CompState::new() advertises every
//! protocol global and defines how each binds. Simple globals
//! go through the `gs!` macro (init + store udata); globals
//! with a real bind-time contract (wl_output's initial event
//! burst, wl_shm's format list, foreign-toplevel's replay of
//! existing windows) get handwritten GlobalDispatch impls.

use anyhow::Context;
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_wm_base;
use wayland_server::protocol::{
    wl_compositor, wl_data_device_manager, wl_output, wl_seat, wl_shm, wl_subcompositor,
};
use wayland_server::{
    Client, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New, Resource,
};

use super::{
    activation, alpha, bell, content_type, copy_capture, cursor_shape, data_control, decoration,
    dialog, dmabuf, ext_data_control, ext_ftl, foreign, foreign_toplevel, fractional_scale, gamma,
    gestures, idle_inhibit, idle_notify, input_inhibit, input_method, kbd_inhibit, layer, outpower,
    output_mgmt, pointer_ext, present, primary_sel, screencopy, session_lock, spb, syncobj, tablet,
    tearing, text_input, viewporter, virtual_input, vkeyboard, ws_ext, xdg_icon, xdg_output,
    CompState,
};

impl CompState {
    /// Bind the listening socket + create globals.
    pub fn new() -> anyhow::Result<(Self, ListeningSocket)> {
        let display = Display::new()?;
        let handle = display.handle();
        handle.create_global::<Self, wl_compositor::WlCompositor, ()>(6, ());
        handle.create_global::<Self, wl_subcompositor::WlSubcompositor, ()>(1, ());
        handle.create_global::<Self, wl_shm::WlShm, ()>(2, ());
        handle.create_global::<Self, xdg_wm_base::XdgWmBase, ()>(6, ());
        handle.create_global::<Self, wl_seat::WlSeat, ()>(9, ());
        handle.create_global::<Self, wl_data_device_manager::WlDataDeviceManager, ()>(3, ());
        // Pointer extensions — relative deltas, lock/confine,
        // themed cursor shapes; games and precision tools need
        // all three for correct, warp-free input.
        handle.create_global::<Self, pointer_ext::ZwpRelativePointerManagerV1, ()>(1, ());
        handle.create_global::<Self, pointer_ext::ZwpPointerConstraintsV1, ()>(1, ());
        handle.create_global::<Self, cursor_shape::WpCursorShapeManagerV1, ()>(1, ());
        // wlr layer shell — panels, wallpapers, launchers and
        // notifications all speak this protocol.
        handle.create_global::<Self, layer::ZwlrLayerShell, ()>(4, ());
        handle.create_global::<Self, screencopy::ZwlrScreencopyManager, ()>(3, ());
        handle.create_global::<Self, xdg_output::ZxdgOutputManager, ()>(3, ());
        handle.create_global::<Self, foreign_toplevel::ZwlrForeignToplevelManager, ()>(3, ());
        handle.create_global::<Self, output_mgmt::ZwlrOutputManager, ()>(4, ());
        handle.create_global::<Self, gamma::ZwlrGammaControlManager, ()>(1, ());
        handle.create_global::<Self, input_inhibit::ZwlrInputInhibitManager, ()>(1, ());
        handle.create_global::<Self, present::WpPresentation, ()>(2, ());
        // dmabuf v4 — tranche feedback carries the GPU's
        // main_device dev_t; XWayland's glamor init resolves
        // its GBM device from it. Without the hint (or the
        // wl_drm fallback below) XWayland renders in software
        // and shovels multi-MB shm buffers at us every frame.
        handle.create_global::<Self, dmabuf::ZwpLinuxDmabufV1, ()>(4, ());
        handle.create_global::<Self, virtual_input::ZwlrVirtualPointerManager, ()>(2, ());
        handle.create_global::<Self, data_control::ZwlrDataControlManager, ()>(2, ());
        handle.create_global::<Self, ext_data_control::ExtDataControlManager, ()>(1, ());
        handle.create_global::<Self, ext_ftl::ExtForeignToplevelList, ()>(1, ());
        handle.create_global::<Self, xdg_icon::XdgIconManager, ()>(1, ());
        handle.create_global::<Self, copy_capture::ExtCopyCaptureManager, ()>(1, ());
        handle.create_global::<Self, copy_capture::ExtOutputSourceManager, ()>(1, ());
        handle.create_global::<Self, copy_capture::ExtForeignToplevelSourceManager, ()>(1, ());
        handle.create_global::<Self, syncobj::WpLinuxDrmSyncobjManagerV1, ()>(1, ());
        handle.create_global::<Self, vkeyboard::ZwpVirtualKeyboardManager, ()>(1, ());
        handle.create_global::<Self, idle_inhibit::ZwpIdleInhibitManager, ()>(1, ());
        handle.create_global::<Self, idle_notify::ExtIdleNotifier, ()>(2, ());
        handle.create_global::<Self, session_lock::ExtSessionLockManager, ()>(1, ());
        handle.create_global::<Self, activation::XdgActivation, ()>(1, ());
        handle.create_global::<Self, viewporter::WpViewporterGlobal, ()>(1, ());
        handle.create_global::<Self, fractional_scale::WpFractionalScaleManager, ()>(1, ());
        handle.create_global::<Self, gestures::ZwpPointerGesturesV1, ()>(3, ());
        handle.create_global::<Self, kbd_inhibit::KbdInhibitManager, ()>(1, ());
        handle.create_global::<Self, text_input::ZwpTextInputManagerV3, ()>(1, ());
        handle.create_global::<Self, input_method::ZwpInputMethodManagerV2, ()>(1, ());
        handle.create_global::<Self, decoration::ZxdgDecorationManagerV1, ()>(1, ());
        handle.create_global::<Self, decoration::KdeDecoManager, ()>(1, ());
        handle.create_global::<Self, foreign::ZxdgExporterV2, ()>(1, ());
        handle.create_global::<Self, foreign::ZxdgExporterV1, ()>(1, ());
        handle.create_global::<Self, foreign::ZxdgImporterV1, ()>(1, ());
        handle.create_global::<Self, foreign::ZxdgImporterV2, ()>(1, ());
        handle.create_global::<Self, spb::WpSpbManager, ()>(1, ());
        handle.create_global::<Self, alpha::WpAlphaManager, ()>(1, ());
        handle.create_global::<Self, content_type::WpContentTypeManager, ()>(1, ());
        handle.create_global::<Self, tearing::WpTearingManager, ()>(1, ());
        handle.create_global::<Self, outpower::ZwlrPowerManager, ()>(1, ());
        handle.create_global::<Self, dialog::XdgWmDialog, ()>(1, ());
        handle.create_global::<Self, bell::XdgSystemBell, ()>(1, ());
        handle.create_global::<Self, tablet::ZwpTabletManager, ()>(2, ());
        handle.create_global::<Self, primary_sel::ZwpPrimarySelectionDeviceManagerV1, ()>(1, ());
        handle.create_global::<Self, ws_ext::ExtWorkspaceManager, ()>(1, ());
        // Pick the first free odin-N socket. The connect probe
        // separates a LIVE compositor (never touch its socket —
        // unlinking it would orphan every future client) from
        // stale files a crashed instance left behind.
        // The primary node's dev_t feeds the dmabuf v4
        // `main_device` event — XWayland's glamor resolves its
        // GBM device from it (the protocol's device-hint job;
        // legacy wl_drm is gone from wayland.xml upstream).
        let drm_devt = crate::backend::drm::primary_devt();
        let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user".into());
        let (name, sock) = (0..10)
            .find_map(|n| {
                let name = format!("odin-{n}");
                if std::os::unix::net::UnixStream::connect(format!("{rt}/{name}")).is_ok() {
                    return None; // live compositor owns this name
                }
                let _ = std::fs::remove_file(format!("{rt}/{name}"));
                let _ = std::fs::remove_file(format!("{rt}/{name}.lock"));
                ListeningSocket::bind(&name).ok().map(|s| (name, s))
            })
            .context("no free odin-N socket")?;
        crate::spawn::set_socket_name(&name);
        info!(socket = %name, "compositor socket listening");
        Ok((
            Self {
                dh: handle.clone(),
                display: Some(display),
                order: Vec::new(),
                popups: Vec::new(),
                xpopups: Vec::new(),
                pointers: Vec::new(),
                keyboards: Vec::new(),
                serial: 1,
                focus: None,
                kbd_focus: None,
                ptr_inside: false,
                ptr_pos: (0.0, 0.0),
                motion_pending: false,
                rel_pending: (0.0, 0.0, 0.0, 0.0),
                pending_release: Vec::new(),
                release_next: Vec::new(),
                release_ready: Vec::new(),
                cursor_surf: None,
                cursor_hotspot: (0, 0),
                cursor_img: None,
                cursor_gen: 0,
                hw_cursor: std::collections::HashSet::new(),
                sw_cursor: true,
                swipe_gestures: Vec::new(),
                pinch_gestures: Vec::new(),
                hold_gestures: Vec::new(),
                text_inputs: Vec::new(),
                im: None,
                im_popups: Vec::new(),
                touch_objs: Vec::new(),
                touch_pts: std::collections::BTreeMap::new(),
                touch_dirty: std::collections::HashSet::new(),
                touch_serials: std::collections::BTreeMap::new(),
                deco_objs: Vec::new(),
                exported: std::collections::BTreeMap::new(),
                imported: Vec::new(),
                imported_v1: Vec::new(),
                cursor_cache: std::collections::HashMap::new(),
                rel_ptrs: Vec::new(),
                constraint: None,
                layers: Vec::new(),
                space: (1920.0, 1200.0),
                outputs: Vec::new(),
                ft_managers: Vec::new(),
                ft_handles: Vec::new(),
                om_managers: Vec::new(),
                om_heads: Vec::new(),
                om_serial: 1,
                out_globals: std::collections::HashMap::new(),
                pending_outops: Vec::new(),
                gamma: std::collections::BTreeMap::new(),
                power: std::collections::BTreeMap::new(),
                tablet_seats: Vec::new(),
                tablets: Vec::new(),
                tools: Vec::new(),
                pads: Vec::new(),
                inhibitor: None,
                damage: Vec::new(),
                damage_full: true,
                damage_taken: false,
                frame_damage: None,
                present_seq: 0,
                vp_axis_source: 0,
                idle_inhibitors: Vec::new(),
                drag: None,
                ptr_down: 0,
                ctrl_devices: Vec::new(),
                ext_ctrl_devices: Vec::new(),
                ext_ftl_lists: Vec::new(),
                ext_ftl_handles: Vec::new(),
                cap_sessions: Vec::new(),
                sync_waiters: Vec::new(),
                tsig_pending: Vec::new(),
                tsig_next: Vec::new(),
                tsig_ready: Vec::new(),
                primary: None,
                prim_devices: Vec::new(),
                ws_clients: Vec::new(),
                cfg: crate::config::load(),
                udev_mods: 0,
                grab: None,
                rs_pending: None,
                repeat_key: None,
                repeat_next: None,
                data_devices: Vec::new(),
                selection: None,
                layout: crate::wm::Layout::new(),
                dirty: true,
                dump_seq: 0,
                quit: false,
                drm_devt,
                drm_fd: None,
                x_paste_q: Vec::new(),
                stat_frames: 0,
                stat_full: 0,
                stat_last: std::time::Instant::now(),
                stat_frame_ms: 0.0,
                stat_input_ms: 0.0,
                stat_render_ms: 0.0,
                stat_present_ms: 0.0,
                stat_copy_ms: 0.0,
                stat_dirty_ms: 0.0,
                stat_flip_ms: 0.0,
                stat_wait_ms: 0.0,
                stat_upload_ms: 0.0,
                stat_draw_ms: 0.0,
                stat_submit_ms: 0.0,
                stat_pump_ms: 0.0,
                stat_xwm_ms: 0.0,
                stat_drain_ms: 0.0,
                stat_up_kb: 0,
                stat_up_texs: 0,
                stat_up_big_kb: 0,
                stat_up_big_w: 0,
                stat_up_big_h: 0,
                last_input: std::time::Instant::now(),
                idle_notifs: Vec::new(),
                lock: None,
                act_tokens: Default::default(),
                all_surfaces: Vec::new(),
                xwins: Default::default(),
                x_display: None,
                xwl_client: None,
                xreqs: Vec::new(),
            },
            sock,
        ))
    }
}

macro_rules! gs {
    ($iface:ty) => {
        impl GlobalDispatch<$iface, ()> for CompState {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$iface>,
                _: &(),
                init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                init.init::<$iface, ()>(resource, ());
            }
        }
    };
}
gs!(wl_compositor::WlCompositor);
gs!(xdg_wm_base::XdgWmBase);
gs!(wl_data_device_manager::WlDataDeviceManager);
gs!(pointer_ext::ZwpRelativePointerManagerV1);
gs!(pointer_ext::ZwpPointerConstraintsV1);
gs!(cursor_shape::WpCursorShapeManagerV1);
gs!(layer::ZwlrLayerShell);
gs!(screencopy::ZwlrScreencopyManager);
gs!(xdg_output::ZxdgOutputManager);
gs!(gamma::ZwlrGammaControlManager);
gs!(input_inhibit::ZwlrInputInhibitManager);
gs!(virtual_input::ZwlrVirtualPointerManager);
gs!(data_control::ZwlrDataControlManager);
gs!(ext_data_control::ExtDataControlManager);
gs!(copy_capture::ExtCopyCaptureManager);
gs!(copy_capture::ExtOutputSourceManager);
gs!(copy_capture::ExtForeignToplevelSourceManager);
gs!(syncobj::WpLinuxDrmSyncobjManagerV1);
gs!(vkeyboard::ZwpVirtualKeyboardManager);
gs!(idle_inhibit::ZwpIdleInhibitManager);
gs!(idle_notify::ExtIdleNotifier);
gs!(session_lock::ExtSessionLockManager);
gs!(activation::XdgActivation);
gs!(viewporter::WpViewporterGlobal);
gs!(fractional_scale::WpFractionalScaleManager);
gs!(gestures::ZwpPointerGesturesV1);
gs!(kbd_inhibit::KbdInhibitManager);
gs!(text_input::ZwpTextInputManagerV3);
gs!(input_method::ZwpInputMethodManagerV2);
gs!(decoration::ZxdgDecorationManagerV1);
gs!(foreign::ZxdgExporterV2);
gs!(foreign::ZxdgExporterV1);
gs!(foreign::ZxdgImporterV1);
gs!(foreign::ZxdgImporterV2);
gs!(spb::WpSpbManager);
gs!(alpha::WpAlphaManager);
gs!(content_type::WpContentTypeManager);
gs!(tearing::WpTearingManager);
gs!(outpower::ZwlrPowerManager);
gs!(dialog::XdgWmDialog);
gs!(bell::XdgSystemBell);
gs!(tablet::ZwpTabletManager);

// Foreign-toplevel is the one global that cannot stay a plain
// gs! bind — a manager connecting after windows already exist
// must get the full list replayed, not just future maps.
impl GlobalDispatch<foreign_toplevel::ZwlrForeignToplevelManager, ()> for CompState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<foreign_toplevel::ZwlrForeignToplevelManager>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let mgr = init.init::<foreign_toplevel::ZwlrForeignToplevelManager, ()>(resource, ());
        state.ft_managers.push(mgr.clone());
        state.ft_replay(&mgr);
    }
}

// Same replay contract for the read-only ext list — a late
// binding taskbar gets the full window set, not just new maps.
impl GlobalDispatch<ext_ftl::ExtForeignToplevelList, ()> for CompState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<ext_ftl::ExtForeignToplevelList>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let list = init.init::<ext_ftl::ExtForeignToplevelList, ()>(resource, ());
        state.ext_ftl_lists.push(list.clone());
        state.ext_ftl_replay(&list);
    }
}

// Icon manager needs the preferred-size burst at bind — the
// spec's `done` is mandatory even when the list is empty.
impl GlobalDispatch<xdg_icon::XdgIconManager, ()> for CompState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<xdg_icon::XdgIconManager>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let mgr = init.init::<xdg_icon::XdgIconManager, ()>(resource, ());
        CompState::xdg_icon_hello(&mgr);
    }
}

// Output-management mirrors foreign-toplevel's bind contract:
// a manager binding late gets the full head set replayed, not
// just future hotplugs.
impl GlobalDispatch<output_mgmt::ZwlrOutputManager, ()> for CompState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<output_mgmt::ZwlrOutputManager>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let mgr = init.init::<output_mgmt::ZwlrOutputManager, ()>(resource, ());
        state.om_managers.push(mgr.clone());
        state.om_replay(&mgr);
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for CompState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let shm = init.init::<wl_shm::WlShm, ()>(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl CompState {
    /// Register one wl_output global per layout output — called
    /// once the backend knows its connectors (new() runs earlier)
    /// and again per hotplug. The GlobalId is kept so a hot-unplug
    /// can withdraw exactly this connector's global; the global's
    /// udata carries the connector name so bind advertises that
    /// specific monitor's real geometry.
    pub fn register_output_globals(&mut self) {
        let handle = self.dh.clone();
        for name in self.layout.outputs.keys().cloned().collect::<Vec<_>>() {
            if self.out_globals.contains_key(&name) {
                continue;
            }
            let gid = handle.create_global::<Self, wl_output::WlOutput, String>(4, name.clone());
            self.out_globals.insert(name, gid);
        }
    }

    /// Hot-unplug: withdraw the connector's wl_output global and
    /// retire every output-management head describing it. Already
    /// bound wl_output resources stay alive until clients release
    /// them after the registry remove — they simply stop
    /// receiving updates.
    pub fn output_removed(&mut self, name: &str) {
        if let Some(gid) = self.out_globals.remove(name) {
            self.dh.remove_global::<Self>(gid);
        }
        self.om_serial = self.om_serial.wrapping_add(1);
        for h in &self.om_heads {
            if h.output == name && h.obj.is_alive() {
                h.obj.finished();
            }
        }
        self.om_heads.retain(|h| h.output != name);
        for m in &self.om_managers {
            if m.is_alive() {
                m.done(self.om_serial);
            }
        }
        // Copy-capture sessions bound to this output are dead —
        // their clients get `stopped`, not silent garbage.
        self.cap_src_died(&copy_capture::CapSrc::Output(name.to_string()));
    }

    /// Hotplug announce: register the wl_output global, then push
    /// a new head to every live output manager — same describe
    /// burst bind-time replay sends.
    pub fn output_added(&mut self, name: &str) {
        let gid = self
            .dh
            .create_global::<Self, wl_output::WlOutput, String>(4, name.to_string());
        self.out_globals.insert(name.to_string(), gid);
        self.om_serial = self.om_serial.wrapping_add(1);
        for m in self.om_managers.clone() {
            if !m.is_alive() {
                continue;
            }
            let Some(client) = m.client() else { continue };
            if let Ok(head) = client.create_resource::<output_mgmt::ZwlrHead, String, CompState>(
                &self.dh,
                m.version().min(4),
                name.to_string(),
            ) {
                m.head(&head);
                self.om_head_describe(&head, name);
                self.om_heads.push(output_mgmt::OmHead {
                    obj: head,
                    output: name.to_string(),
                });
            }
            m.done(self.om_serial);
        }
    }
}

impl GlobalDispatch<wl_output::WlOutput, String> for CompState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_output::WlOutput>,
        name: &String,
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        // The connector name rides on the resource — session-lock
        // resolves get_lock_surface's wl_output against it.
        let out = init.init::<wl_output::WlOutput, String>(resource, name.clone());
        let v = out.version();
        // This global describes exactly one connector — its real
        // position, mode list, physical size and EDID identity,
        // not a shared primary guess.
        let info = state.layout.outputs.get(name).cloned();
        let (x, y) = info.as_ref().map(|o| (o.x, o.y)).unwrap_or((0, 0));
        let (pw, ph) = info.as_ref().map(|o| o.phys_mm).unwrap_or((0, 0));
        let (make, model) = info
            .as_ref()
            .map(|o| {
                (
                    if o.edid.make.is_empty() {
                        "unknown".to_string()
                    } else {
                        o.edid.make.clone()
                    },
                    if o.edid.model.is_empty() {
                        o.name.clone()
                    } else {
                        o.edid.model.clone()
                    },
                )
            })
            .unwrap_or_else(|| ("unknown".to_string(), name.clone()));
        out.geometry(
            x,
            y,
            pw as i32,
            ph as i32,
            wl_output::Subpixel::Unknown,
            make,
            model,
            wl_output::Transform::Normal,
        );
        // One mode event per advertised connector mode — clients
        // offering a mode switcher need the full list, not just
        // the current mode.
        let modes = info.as_ref().map(|o| o.modes.clone()).unwrap_or_default();
        if modes.is_empty() {
            let (w, h, hz) = info.as_ref().map(|o| o.mode).unwrap_or((1280, 800, 60));
            out.mode(
                wl_output::Mode::Current | wl_output::Mode::Preferred,
                w as i32,
                h as i32,
                (hz * 1000) as i32,
            );
        } else {
            let cur = info.as_ref().map(|o| o.mode).unwrap_or_default();
            for m in &modes {
                let mut flags = wl_output::Mode::empty();
                if (m.w, m.h, m.hz) == cur {
                    flags |= wl_output::Mode::Current;
                }
                if m.preferred {
                    flags |= wl_output::Mode::Preferred;
                }
                out.mode(flags, m.w as i32, m.h as i32, (m.hz * 1000) as i32);
            }
        }
        // scale is a v2 event — always integer 1 until fractional
        // scaling lands.
        if v >= 2 {
            out.scale(1);
        }
        // name/description are REQUIRED events on v4 binds —
        // skipping them crashes clients like swaybg.
        if v >= 4 {
            out.name(name.clone());
            out.description(format!("Odin output {name}"));
        }
        out.done();
        state.outputs.push((out, name.clone()));
        info!(output = %name, "wl_output advertised");
    }
}

impl Dispatch<wl_output::WlOutput, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
    }
}

/// wl_output carries no client-visible requests — this second
/// udata flavour exists because bound resources store the
/// connector name for session-lock's output resolution.
impl Dispatch<wl_output::WlOutput, String> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &String,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
    }
}

/// KDE decoration manager — `default_mode` rides along on the
/// bind, so it needs a real bind impl rather than gs!.
impl GlobalDispatch<decoration::KdeDecoManager, ()> for CompState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<decoration::KdeDecoManager>,
        _: &(),
        init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let mgr = init.init::<decoration::KdeDecoManager, ()>(resource, ());
        mgr.default_mode(
            wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Client,
        );
    }
}
