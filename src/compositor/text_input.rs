// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_text_input_v3 — the application-facing half of IME.
//! A client gets one text_input object per seat, enables it on
//! its focused surface and describes the text field (surrounding
//! text, content type, cursor rect). The compositor feeds that
//! state to the input method (input-method-v2) and routes the
//! IM's preedit/commit strings back to the text_input.

use std::sync::Mutex;
use wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3 as tim, zwp_text_input_v3 as ti3,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::CompState;

// Re-exports — mod.rs/globals.rs name these through this path.
pub use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
pub use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ZwpTextInputV3;

/// Everything a text field told us — forwarded to the input
/// method on activation. `enter`/`leave` track which surface
/// this object is enabled on.
#[derive(Default)]
pub struct TiData {
    /// Surface this text_input last entered — enable() binds to it.
    pub entered: Mutex<Option<wl_surface::WlSurface>>,
    /// enable+commit happened — the field wants IM service.
    pub enabled: Mutex<bool>,
    /// surrounding_text(text, cursor, anchor).
    pub surrounding: Mutex<(String, u32, u32)>,
    /// text_change_cause — 0=input_method, 1=other.
    pub cause: Mutex<u32>,
    /// content_type(hint, purpose) bitfields, passed through raw.
    pub content: Mutex<(u32, u32)>,
    /// Cursor rectangle in surface-local coords — the IM popup
    /// anchors here.
    pub cursor_rect: Mutex<(i32, i32, i32, i32)>,
    /// done-serial counter — counts issued commit requests.
    pub serial: Mutex<u32>,
}

impl Dispatch<ZwpTextInputManagerV3, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTextInputManagerV3,
        request: tim::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            tim::Request::GetTextInput { id, .. } => {
                let ti = init.init::<ZwpTextInputV3, TiData>(id, TiData::default());
                // A field created on an ALREADY-focused surface
                // gets its enter right now — focus transitions
                // alone would leave it dangling until the next
                // window switch.
                if let Some(focused) = state
                    .kbd_focus
                    .as_ref()
                    .filter(|s| s.client() == ti.client())
                    .cloned()
                {
                    *ti.data::<TiData>().unwrap().entered.lock().unwrap() = Some(focused.clone());
                    ti.enter(&focused);
                }
                state.text_inputs.push(ti);
            }
            tim::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpTextInputV3, TiData> for CompState {
    fn request(
        state: &mut Self,
        client: &Client,
        ti: &ZwpTextInputV3,
        request: ti3::Request,
        data: &TiData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ti3::Request::Enable => {
                // Bind to the surface this object last entered —
                // that's per spec the only surface enable may
                // apply to.
                if data.entered.lock().unwrap().is_some() {
                    *data.enabled.lock().unwrap() = true;
                }
            }
            ti3::Request::Disable => {
                *data.enabled.lock().unwrap() = false;
            }
            ti3::Request::SetSurroundingText {
                text,
                cursor,
                anchor,
            } => {
                *data.surrounding.lock().unwrap() = (text, cursor as u32, anchor as u32);
            }
            ti3::Request::SetTextChangeCause { cause } => {
                if let Ok(c) = cause.into_result() {
                    *data.cause.lock().unwrap() = u32::from(c);
                }
            }
            ti3::Request::SetContentType { hint, purpose } => {
                let h = match hint {
                    wayland_server::WEnum::Value(v) => u32::from(v),
                    _ => 0,
                };
                let p = match purpose {
                    wayland_server::WEnum::Value(v) => u32::from(v),
                    _ => 0,
                };
                *data.content.lock().unwrap() = (h, p);
            }
            ti3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                *data.cursor_rect.lock().unwrap() = (x, y, width, height);
            }
            ti3::Request::Commit => {
                *data.serial.lock().unwrap() += 1;
                // A commit while enabled is a full state push to
                // the IM — surrounding, cause, content type and
                // the done counter all flow.
                if *data.enabled.lock().unwrap() {
                    state.im_ti_state(ti, data);
                } else {
                    state.im_ti_deactivate(ti);
                }
            }
            ti3::Request::Destroy => {
                state.im_ti_deactivate(ti);
                state.text_inputs.retain(|t| t.id() != ti.id());
            }
            _ => {}
        }
        let _ = client;
    }
}

impl CompState {
    /// Keyboard-focus moved — text_inputs on the losing surface
    /// get leave (and the IM a deactivate), the winning surface's
    /// objects get enter. Called from set_kbd_focus.
    pub fn ti_focus(&mut self, new_focus: Option<&wl_surface::WlSurface>) {
        self.text_inputs.retain(|t| t.is_alive());
        for ti in self.text_inputs.clone() {
            let Some(data) = ti.data::<TiData>() else {
                continue;
            };
            let mut entered = data.entered.lock().unwrap();
            match (&*entered, new_focus) {
                // Entered the previously-focused surface's ti —
                // but ONLY surfaces this client owns ever see
                // enter; foreign surfaces act like no focus.
                (old, Some(new))
                    if old.as_ref().is_none_or(|o| o.id() != new.id())
                        && ti.client() == new.client() =>
                {
                    if let Some(old) = old.as_ref() {
                        ti.leave(old);
                        if *data.enabled.lock().unwrap() {
                            self.im_ti_deactivate(&ti);
                            *data.enabled.lock().unwrap() = false;
                        }
                    }
                    *entered = Some(new.clone());
                    ti.enter(new);
                }
                // Focus went nowhere — every entered object leaves.
                (Some(old), None) => {
                    ti.leave(old);
                    *entered = None;
                    if *data.enabled.lock().unwrap() {
                        self.im_ti_deactivate(&ti);
                    }
                }
                _ => {}
            }
        }
    }
}
