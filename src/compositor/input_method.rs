// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! zwp_input_method_v2 — the IME-facing half. One input method
//! per seat observes the focused text_input's state
//! (activate/deactivate/surrounding/content_type/done) and
//! writes back preedit/commit strings plus surrounding-text
//! deletions, which the compositor forwards to the text_input.
//!
//! Side objects: a keyboard grab (the IM composes key events
//! itself) and popup surfaces for candidate windows, anchored
//! at the text field's cursor rectangle.

use std::sync::Mutex;
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols_misc::zwp_input_method_v2::server::{
    zwp_input_method_keyboard_grab_v2 as kgrab, zwp_input_method_manager_v2 as imm,
    zwp_input_method_v2 as im2, zwp_input_popup_surface_v2 as imps,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::text_input::{TiData, ZwpTextInputV3 as Ti3};
use super::CompState;

// Re-exports — globals.rs names the manager through this path.
pub use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2;
pub use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
pub use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_v2::ZwpInputMethodV2;
pub use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2;

/// Pending IM → text_input operations — applied atomically on
/// `commit`, then cleared. Mirrors the protocol's double
/// buffering.
#[derive(Default)]
pub struct ImData {
    /// Text the IM wants committed at the cursor.
    pub commit_string: Mutex<Option<String>>,
    /// (text, cursor_begin, cursor_end) — preedit styling.
    pub preedit: Mutex<Option<(String, i32, i32)>>,
    /// (before, after) — surrounding delete pending.
    pub delete: Mutex<Option<(u32, u32)>>,
    /// The text_input currently activated — the commit target.
    pub active_ti: Mutex<Option<Ti3>>,
    /// Grab object once the IM takes the seat's keyboard.
    pub grab: Mutex<Option<ZwpInputMethodKeyboardGrabV2>>,
    /// done-event counter — the IM's serial space.
    pub done_serial: Mutex<u32>,
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpInputMethodManagerV2,
        request: imm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            imm::Request::GetInputMethod { input_method, .. } => {
                let im = init.init::<ZwpInputMethodV2, ImData>(input_method, ImData::default());
                // One IM per seat — a second grabs the slot and
                // the previous one gets `unavailable`, the
                // protocol's own eviction notice.
                if let Some(old) = state.im.replace(im) {
                    if old.is_alive() {
                        old.unavailable();
                    }
                }
            }
            imm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ImData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        im: &ZwpInputMethodV2,
        request: im2::Request,
        data: &ImData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            im2::Request::CommitString { text } => {
                *data.commit_string.lock().unwrap() = Some(text);
            }
            im2::Request::SetPreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                *data.preedit.lock().unwrap() = Some((text, cursor_begin, cursor_end));
            }
            im2::Request::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                *data.delete.lock().unwrap() = Some((before_length, after_length));
            }
            im2::Request::Commit { .. } => {
                // Atomic apply: deletion first, then commit string
                // — the order the protocol documents.
                let Some(ti) = data.active_ti.lock().unwrap().clone() else {
                    return;
                };
                if !ti.is_alive() {
                    return;
                }
                if let Some((before, after)) = data.delete.lock().unwrap().take() {
                    ti.delete_surrounding_text(before, after);
                }
                if let Some(text) = data.commit_string.lock().unwrap().take() {
                    ti.commit_string(Some(text));
                }
                if let Some((text, cb, ce)) = data.preedit.lock().unwrap().take() {
                    ti.preedit_string(Some(text), cb, ce);
                }
                // The ti's own serial — clients correlate done
                // with THEIR commit count, not ours.
                let serial = ti
                    .data::<TiData>()
                    .map(|d| *d.serial.lock().unwrap())
                    .unwrap_or(0);
                ti.done(serial);
            }
            im2::Request::GetInputPopupSurface { id, surface } => {
                let pop =
                    init.init::<ZwpInputPopupSurfaceV2, wl_surface::WlSurface>(id, surface.clone());
                // Position comes from the active ti's cursor
                // rect — replay it so the popup lands right even
                // if created after the last update.
                if let Some(rect) = data
                    .active_ti
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|t| t.data::<TiData>())
                    .map(|d| *d.cursor_rect.lock().unwrap())
                {
                    pop.text_input_rectangle(rect.0, rect.1, rect.2, rect.3);
                }
                state.im_popups.push(super::ImPopup {
                    surf: surface,
                    obj: pop,
                });
            }
            im2::Request::GrabKeyboard { keyboard } => {
                let g = init.init::<ZwpInputMethodKeyboardGrabV2, ()>(keyboard, ());
                // The grabbed client needs the keymap before it
                // can make sense of key events — same blob the
                // real wl_keyboard path shares.
                if let Some((fd, size)) = super::keymap::keymap_fd(&state.cfg) {
                    use std::os::unix::io::AsFd;
                    g.keymap(
                        wayland_server::protocol::wl_keyboard::KeymapFormat::XkbV1,
                        fd.as_fd(),
                        size,
                    );
                }
                *data.grab.lock().unwrap() = Some(g);
            }
            im2::Request::Destroy => {
                data.grab.lock().unwrap().take();
                if state.im.as_ref().is_some_and(|o| o.id() == im.id()) {
                    state.im = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputPopupSurfaceV2, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwpInputPopupSurfaceV2,
        _request: imps::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` exists — drop the scene record.
        state
            .im_popups
            .retain(|p| p.surf.id() != surf.id() && p.obj.id() != obj.id());
    }
}

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        obj: &ZwpInputMethodKeyboardGrabV2,
        request: kgrab::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // `release` is a destructor — the request arriving IS the
        // release; nothing to signal back. A stale grab pointer in
        // ImData self-heals via the is_alive check in im_key.
        let _ = obj;
        let _ = request;
    }
}

impl CompState {
    /// A text_input enabled+committed → activate the IM and push
    /// the whole field state (surrounding, cause, content type,
    /// cursor rect) followed by done.
    pub fn im_ti_state(&mut self, ti: &ZwpTextInputV3, data: &TiData) {
        let Some(im) = self.im.as_ref().filter(|o| o.is_alive()).cloned() else {
            return;
        };
        let Some(imd) = im.data::<ImData>() else {
            return;
        };
        let was = imd.active_ti.lock().unwrap().is_some();
        if !was {
            im.activate();
        }
        *imd.active_ti.lock().unwrap() = Some(ti.clone());
        let (text, cur, anc) = data.surrounding.lock().unwrap().clone();
        if !text.is_empty() {
            im.surrounding_text(text, cur, anc);
        }
        let cause = *data.cause.lock().unwrap();
        if let Ok(c) =
            wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ChangeCause::try_from(
                cause,
            )
        {
            im.text_change_cause(c);
        }
        let (hint, purpose) = *data.content.lock().unwrap();
        if (hint, purpose) != (0, 0) {
            if let (Ok(h), Ok(p)) = (
                wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ContentHint::try_from(hint),
                wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ContentPurpose::try_from(purpose),
            ) {
                im.content_type(h, p);
            }
        }
        // Cursor rect also reaches every live popup — candidates
        // anchor under the caret, not mid-window.
        let rect = *data.cursor_rect.lock().unwrap();
        for pop in &self.im_popups {
            if pop.obj.is_alive() {
                pop.obj.text_input_rectangle(rect.0, rect.1, rect.2, rect.3);
            }
        }
        *imd.done_serial.lock().unwrap() += 1;
        im.done();
    }

    /// The focused text_input disabled/left → the IM goes
    /// inactive; popups must hide until the next activate.
    pub fn im_ti_deactivate(&mut self, _ti: &ZwpTextInputV3) {
        let Some(im) = self.im.as_ref().filter(|o| o.is_alive()).cloned() else {
            return;
        };
        if let Some(imd) = im.data::<ImData>() {
            if imd.active_ti.lock().unwrap().take().is_some() {
                im.deactivate();
            }
        }
    }

    /// Hardware key while an IM grab is live → the grab client,
    /// not the focus surface. Returns true when consumed.
    pub fn im_key(&mut self, key: u32, pressed: bool) -> bool {
        let Some(grab) = self
            .im
            .as_ref()
            .and_then(|im| im.data::<ImData>())
            .and_then(|d| d.grab.lock().unwrap().clone())
        else {
            return false;
        };
        if !grab.is_alive() {
            return false;
        }
        let serial = self.next_serial_pub();
        grab.key(
            serial,
            super::now_ms(),
            key,
            if pressed {
                wayland_server::protocol::wl_keyboard::KeyState::Pressed
            } else {
                wayland_server::protocol::wl_keyboard::KeyState::Released
            },
        );
        true
    }

    /// Modifier mask → the grab, mirroring push_modifiers.
    pub fn im_modifiers(&mut self) -> bool {
        let Some(grab) = self
            .im
            .as_ref()
            .and_then(|im| im.data::<ImData>())
            .and_then(|d| d.grab.lock().unwrap().clone())
        else {
            return false;
        };
        if !grab.is_alive() {
            return false;
        }
        let serial = self.next_serial_pub();
        let (dep, lat, lck) = super::keymap::mods_to_xkb(self.udev_mods);
        grab.modifiers(serial, dep, lat, lck, 0);
        true
    }
}
