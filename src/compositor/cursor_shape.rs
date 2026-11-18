// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! wp_cursor_shape — server-side themed pointer images.
//!
//! Clients name a shape (text, pointer, resize handles, dnd
//! feedback…); the compositor resolves it against the system
//! Xcursor theme and draws it itself. The hotspot lives inside
//! the image, so clicks land exactly where the cursor visually
//! points — and every client gets consistent, HiDPI-correct
//! cursors without shipping its own sprite set.

use wayland_protocols::wp::cursor_shape::v1::server::{
    wp_cursor_shape_device_v1 as csd, wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1 as csm,
};
// Re-export — mod.rs names the global through this module.
pub use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, WEnum};

use super::CompState;

/// wp_cursor_shape shape enum → Xcursor theme names, most
/// preferred first — themes disagree on naming so every shape
/// carries the freedesktop name plus legacy fallbacks.
fn shape_names(shape: WEnum<csd::Shape>) -> &'static [&'static str] {
    use csd::Shape as S;
    match shape {
        WEnum::Value(S::ContextMenu) => &["context-menu", "left_ptr"],
        WEnum::Value(S::Help) => &["help", "question_arrow"],
        WEnum::Value(S::Pointer) => &["pointer", "hand2", "hand1"],
        WEnum::Value(S::Progress) => &["progress", "left_ptr_watch", "half-busy"],
        WEnum::Value(S::Wait) => &["wait", "watch"],
        WEnum::Value(S::Cell) => &["cell", "crosshair"],
        WEnum::Value(S::Crosshair) => &["crosshair", "cross"],
        WEnum::Value(S::Text) => &["text", "xterm"],
        WEnum::Value(S::VerticalText) => &["vertical-text"],
        WEnum::Value(S::Alias) => &["alias", "dnd-link"],
        WEnum::Value(S::Copy) => &["copy", "dnd-copy"],
        WEnum::Value(S::Move) => &["move", "dnd-move"],
        WEnum::Value(S::NoDrop) => &["no-drop", "dnd-no-drop", "dnd-none"],
        WEnum::Value(S::NotAllowed) => &["not-allowed", "crossed_circle"],
        WEnum::Value(S::Grab) => &["grab", "openhand", "hand1"],
        WEnum::Value(S::Grabbing) => &["grabbing", "closedhand"],
        WEnum::Value(S::EResize) => &["e-resize", "right_side"],
        WEnum::Value(S::NResize) => &["n-resize", "top_side"],
        WEnum::Value(S::NeResize) => &["ne-resize", "top_right_corner"],
        WEnum::Value(S::NwResize) => &["nw-resize", "top_left_corner"],
        WEnum::Value(S::SResize) => &["s-resize", "bottom_side"],
        WEnum::Value(S::SeResize) => &["se-resize", "bottom_right_corner"],
        WEnum::Value(S::SwResize) => &["sw-resize", "bottom_left_corner"],
        WEnum::Value(S::WResize) => &["w-resize", "left_side"],
        WEnum::Value(S::EwResize) => &["ew-resize", "col-resize", "sb_h_double_arrow"],
        WEnum::Value(S::NsResize) => &["ns-resize", "row-resize", "sb_v_double_arrow"],
        WEnum::Value(S::NeswResize) => &["nesw-resize"],
        WEnum::Value(S::NwseResize) => &["nwse-resize"],
        WEnum::Value(S::ColResize) => &["col-resize", "sb_h_double_arrow"],
        WEnum::Value(S::RowResize) => &["row-resize", "sb_v_double_arrow"],
        WEnum::Value(S::AllScroll) => &["all-scroll", "fleur"],
        WEnum::Value(S::ZoomIn) => &["zoom-in"],
        WEnum::Value(S::ZoomOut) => &["zoom-out"],
        WEnum::Value(S::DndAsk) => &["dnd-ask", "dnd-copy"],
        WEnum::Value(S::AllResize) => &["all-resize", "size_all", "fleur"],
        _ => &["default", "left_ptr"],
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpCursorShapeManagerV1,
        request: csm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            csm::Request::GetPointer {
                cursor_shape_device,
                ..
            }
            | csm::Request::GetTabletToolV2 {
                cursor_shape_device,
                ..
            } => {
                init.init::<WpCursorShapeDeviceV1, ()>(cursor_shape_device, ());
            }
            csm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for CompState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &WpCursorShapeDeviceV1,
        request: csd::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            csd::Request::SetShape { shape, .. } => {
                // Same rule as wl_pointer.set_cursor — only the
                // client under the pointer may shape it; a
                // background client's shape device must not
                // hijack the pointer image.
                if state
                    .focus
                    .as_ref()
                    .is_none_or(|f| !state.client_matches(f, Some(client.clone())))
                {
                    return;
                }
                // Last request wins — a shape overrides whatever
                // cursor the client set before; set_cursor on
                // wl_pointer clears this again (see seat.rs).
                for name in shape_names(shape) {
                    let key = (name.to_string(), 24u32);
                    let img = state
                        .cursor_cache
                        .entry(key)
                        .or_insert_with(|| crate::xcursor::load(name, 24))
                        .clone();
                    if let Some(img) = img {
                        state.cursor_img = Some(img);
                        state.cursor_gen += 1;
                        // The cursor image changes in place —
                        // a 128px rect at the pointer covers
                        // old and new glyphs.
                        let (px, py) = (state.ptr_pos.0 as i32, state.ptr_pos.1 as i32);
                        state.damage.push((px - 64, py - 64, 128, 128));
                        state.dirty = true;
                        return;
                    }
                }
                tracing::warn!("cursor shape: no theme image found");
                state.cursor_img = None;
                state.cursor_gen += 1;
            }
            csd::Request::Destroy => {}
            _ => {}
        }
    }
}
