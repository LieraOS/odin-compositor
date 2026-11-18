// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwlr_layer_shell_v1 protocol glue — the Dispatch impls for
//! the global (get_layer_surface) and per-surface requests.
//! Placement, exclusive zones and hit-testing live in
//! `layer.rs`; this file only moves request state across.

use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1 as ls, zwlr_layer_shell_v1::ZwlrLayerShellV1, zwlr_layer_surface_v1 as lsf,
    zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::layer::{edge, Layer, LayerKind};
use super::{CompState, SurfaceData};

/// The global object — clients bind it then call
/// get_layer_surface per layer surface they want.
impl Dispatch<ZwlrLayerShellV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrLayerShellV1,
        request: ls::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ls::Request::GetLayerSurface {
                id,
                surface,
                output,
                layer,
                ..
            } => {
                let obj = init.init::<ZwlrLayerSurfaceV1, ()>(id, ());
                let kind = match layer {
                    WEnum::Value(ls::Layer::Background) => LayerKind::Background,
                    WEnum::Value(ls::Layer::Bottom) => LayerKind::Bottom,
                    WEnum::Value(ls::Layer::Top) => LayerKind::Top,
                    _ => LayerKind::Overlay,
                };
                state.layers.push(Layer {
                    surf: surface,
                    obj: obj.clone(),
                    kind,
                    anchor: 0,
                    exclusive: 0,
                    margin: (0, 0, 0, 0),
                    size: (0, 0),
                    kbd: 0,
                    pending: Vec::new(),
                    configured: false,
                    excl_edge: 0,
                    rect: (0, 0, 0, 0),
                    output,
                });
                // Initial configure — the client must ack it and
                // commit a buffer before it may map.
                let serial = state.next_serial_pub();
                if let Some(l) = state.layers.last_mut() {
                    l.pending.push(serial);
                    l.obj.configure(serial, 0, 0);
                }
            }
            ls::Request::Destroy => {}
            _ => {}
        }
    }
}

/// The per-surface object — carries anchors, margins, exclusive
/// zones and the configure handshake.
impl Dispatch<ZwlrLayerSurfaceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwlrLayerSurfaceV1,
        request: lsf::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(i) = state.layers.iter().position(|l| l.obj.id() == obj.id()) else {
            return;
        };
        match request {
            lsf::Request::SetSize { width, height } => {
                state.layers[i].size = (width, height);
                state.reconfigure_layer(i);
            }
            lsf::Request::SetAnchor { anchor } => {
                if let WEnum::Value(a) = anchor {
                    state.layers[i].anchor = a.iter().fold(0u32, |acc, e| acc | edge(e));
                }
                state.reconfigure_layer(i);
            }
            lsf::Request::SetExclusiveZone { zone } => {
                state.layers[i].exclusive = zone;
                state.reconfigure_layer(i);
            }
            lsf::Request::SetExclusiveEdge { edge: e } => {
                // v5: an explicit edge the zone applies to —
                // overrides the anchor-derived edge.
                if let WEnum::Value(e) = e {
                    state.layers[i].excl_edge = e.iter().fold(0u32, |acc, b| acc | edge(b));
                }
            }
            lsf::Request::SetMargin {
                top,
                right,
                bottom,
                left,
            } => {
                state.layers[i].margin = (top, right, bottom, left);
                state.reconfigure_layer(i);
            }
            lsf::Request::SetKeyboardInteractivity {
                keyboard_interactivity,
            } => {
                state.layers[i].kbd = match keyboard_interactivity {
                    WEnum::Value(lsf::KeyboardInteractivity::Exclusive) => 1,
                    WEnum::Value(lsf::KeyboardInteractivity::OnDemand) => 2,
                    _ => 0,
                };
            }
            lsf::Request::GetPopup { popup } => {
                // GTK-style layer popups: created earlier via
                // xdg_surface.get_popup with a NULL parent (kept
                // alive by the placeholder parent), reparented to
                // this layer surface here — hit-tests, focus and
                // the scene list all resolve against the parent.
                let layer_surf = state.layers[i].surf.clone();
                if let Some(p) = state.popups.iter_mut().find(|p| p.obj.id() == popup.id()) {
                    p.parent = layer_surf;
                }
            }
            lsf::Request::AckConfigure { serial } => {
                let l = &mut state.layers[i];
                l.pending.retain(|s| *s != serial);
                l.configured = true;
                let surf = l.surf.clone();
                // A client may attach+commit before acking — the
                // early-return in commit_layer left it unmapped;
                // re-drive the map path now the ack landed.
                let has_buf = surf
                    .data::<SurfaceData>()
                    .map(|sd| sd.shown_buf.lock().unwrap().is_some())
                    .unwrap_or(false);
                if has_buf {
                    state.commit_layer(&surf);
                }
            }
            lsf::Request::SetLayer { layer } => {
                state.layers[i].kind = match layer {
                    WEnum::Value(ls::Layer::Background) => LayerKind::Background,
                    WEnum::Value(ls::Layer::Bottom) => LayerKind::Bottom,
                    WEnum::Value(ls::Layer::Top) => LayerKind::Top,
                    _ => LayerKind::Overlay,
                };
                state.damage_everything();
            }
            lsf::Request::Destroy => {
                state.layers.remove(i);
                state.damage_everything();
                state.apply_layout();
            }
            _ => {}
        }
    }
}
