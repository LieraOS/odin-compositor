// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! XKB keymap plumbing for wl_keyboard and IM keyboard grabs —
//! composes the configured layout to a memfd once per request so
//! each client/grab gets its own fd (keymap consumes it).

use crate::config::Config;
use rustix::fd::OwnedFd;

/// Build an XKB keymap for the configured layout, write it to a
/// memfd, hand out fd+size — the payload `wl_keyboard.keymap` takes.
pub(crate) fn keymap_fd(cfg: &Config) -> Option<(OwnedFd, u32)> {
    use xkbcommon::xkb;
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let km = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        &cfg.keymap,
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    let text = km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let bytes = text.as_bytes();
    let fd = rustix::fs::memfd_create("odin-keymap", rustix::fs::MemfdFlags::empty()).ok()?;
    rustix::fs::ftruncate(&fd, bytes.len() as u64).ok()?;
    rustix::io::write(&fd, bytes).ok()?;
    Some((fd, bytes.len() as u32))
}

/// Convert our C/A/S/W modifier mask to xkb modifier indices —
/// the bit layout wl_keyboard.modifiers carries is client-keymap
/// dependent, so we keep xkb's standard index constants here.
/// xkb standard mod indices: Shift=0, Caps=1, Ctrl=2, Alt=3,
/// Mod2(NumLock)=4, Mod3=5, Super=6, Mod5=7.
pub fn mods_to_xkb(mods: u32) -> (u32, u32, u32) {
    let mut dep = 0u32;
    if mods & 4 != 0 {
        dep |= 1 << 0; // Shift
    }
    if mods & 1 != 0 {
        dep |= 1 << 2; // Ctrl
    }
    if mods & 2 != 0 {
        dep |= 1 << 3; // Alt
    }
    if mods & 8 != 0 {
        dep |= 1 << 6; // Super (Mod4)
    }
    (dep, 0, 0)
}
