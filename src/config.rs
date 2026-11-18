// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Keybinding + settings config — loaded at startup and
//! editable live.
//!
//! Format (one binding per line):
//!   C-A-t = spawn weston-terminal
//!   A-Tab = cycle-focus
//!   C-A-q = quit
//! Modifiers: C=Ctrl A=Alt S=Shift W=Super. Unknown lines are
//! logged and skipped — a bad config never kills the compositor.

use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::{info, warn};

/// Server-side decoration policy — `decoration = server`
/// frames every toplevel that does not negotiate client-side,
/// `client` keeps negotiating CSD for everything (the frame
/// stays client-drawn), `auto` honors whichever mode the
/// client requests and defaults undecided clients to SSD.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecoPref {
    Server,
    Client,
    Auto,
}

/// All bindings keyed by (mods, key) for O(log n) lookup.
#[derive(Debug)]

pub struct Config {
    pub bindings: BTreeMap<(u32, u32), String>,
    /// Decoration preference — see `DecoPref`.
    pub deco: DecoPref,
    /// XKB layout name handed to clients via wl_keyboard.keymap —
    /// a `keymap = de` line in bindings.conf overrides it.
    pub keymap: String,
    /// Per-class libinput device settings from input.conf —
    /// applied to every device on add so pointer feel matches
    /// the user's session instead of bare libinput defaults.
    pub input: crate::input_conf::InputConf,
    /// `primary = DP-3` — which connector holds the primary flag
    /// (origin placement, fullscreen fallback, RandR primary).
    pub primary: Option<String>,
    /// `policy.HDMI-A-1 = tiling` — per-output layout policy,
    /// the user's free choice per monitor. Missing entries mean
    /// the global default (Floating).
    pub out_policy: BTreeMap<String, crate::wm::LayoutPolicy>,
    /// `policy = tiling|floating` — the all-monitors shorthand
    /// a settings UI writes for "tile everything"; per-output
    /// `policy.<name>` entries still win for their connector.
    pub default_policy: Option<crate::wm::LayoutPolicy>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            keymap: "de".to_string(),
            input: crate::input_conf::InputConf::load(),
            primary: None,
            out_policy: BTreeMap::new(),
            default_policy: None,
            deco: DecoPref::Auto,
        }
    }
}

/// Named key aliases → evdev codes (the common set; extend as needed).
fn keycode(name: &str) -> Option<u32> {
    Some(match name {
        "Tab" => 15,
        "Enter" | "Return" => 28,
        "Escape" => 1,
        "Space" => 57,
        "Backspace" => 14,
        "Delete" => 111,
        // evdev codes for the letter row — QWERTY order, kernel
        // keycodes (input-event-codes.h).
        "q" | "Q" => 16,
        "w" | "W" => 17,
        "e" | "E" => 18,
        "r" | "R" => 19,
        "t" | "T" => 20,
        "y" | "Y" => 21,
        "u" | "U" => 22,
        "i" | "I" => 23,
        "o" | "O" => 24,
        "p" | "P" => 25,
        "a" | "A" => 30,
        "s" | "S" => 31,
        "d" | "D" => 32,
        "f" | "F" => 33,
        "g" | "G" => 34,
        "h" | "H" => 35,
        "j" | "J" => 36,
        "k" | "K" => 37,
        "l" | "L" => 38,
        "z" | "Z" => 44,
        "x" | "X" => 45,
        "c" | "C" => 46,
        "v" | "V" => 47,
        "b" | "B" => 48,
        "n" | "N" => 49,
        "m" | "M" => 50,
        "Left" => 105,
        "Right" => 106,
        "Up" => 103,
        "Down" => 108,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        _ => return None,
    })
}

/// Parse "C-A-t" → (mods mask, keycode). Returns None on garbage.
fn parse_key(spec: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let parts: Vec<&str> = spec.split('-').collect();
    let (key_name, mod_parts) = parts.split_last()?;
    for m in mod_parts {
        mods |= match *m {
            "C" => 1,
            "A" => 2,
            "S" => 4,
            "W" => 8,
            _ => return None,
        };
    }
    Some((mods, keycode(key_name)?))
}

/// Where the config lives — follows the XDG convention.
pub fn path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".config"));
    base.join("odin/bindings.conf")
}

/// Load bindings; missing file → sensible defaults (never fatal).
pub fn load() -> Config {
    let p = path();
    let mut cfg = Config::default();
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => {
            // Defaults: the compositor stays usable out of the box.
            let mut defaults = "C-A-t = spawn weston-terminal\n\
                            C-A-c = spawn weston-clickdot\n\
                            A-Tab = cycle-focus\n\
                            C-A-Space = toggle-layout\
                            C-A-d = dump-frame\n\
                            C-A-q = quit\n\
                            C-A-Backspace = quit\n"
                .to_string();
            // Workspace bindings: Super+1..9 switches, Super+
            // Shift+1..9 moves the focused window there.
            for i in 1..=9u8 {
                // evdev digits: key 2..=10 for 1..=9.
                defaults.push_str(&format!("W-{} = ws-{i}\n", i + 1));
                defaults.push_str(&format!("W-S-{} = mv-{i}\n", i + 1));
            }
            let defaults: &str = &defaults;
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(&p, defaults);
            info!(?p, "wrote default keybindings");
            defaults.to_string()
        }
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        // Non-binding settings: `keymap = de` picks the XKB layout
        // every client's wl_keyboard.keymap gets.
        if let Some(rest) = line.strip_prefix("keymap") {
            if let Some((_, v)) = rest.split_once('=') {
                cfg.keymap = v.trim().to_string();
                info!(keymap = %cfg.keymap, "keyboard layout configured");
                continue;
            }
        }
        // `primary = DP-3` — user's primary-monitor pick.
        if let Some(rest) = line.strip_prefix("primary") {
            if let Some((_, v)) = rest.split_once('=') {
                cfg.primary = Some(v.trim().to_string());
                info!(primary = %v.trim(), "primary output configured");
                continue;
            }
        }
        // `policy.HDMI-A-1 = tiling|floating` — per-monitor
        // layout policy, the free-choice contract.
        if let Some(name) = line.strip_prefix("policy.") {
            if let Some((_, v)) = name.split_once('=') {
                let name = name.trim().to_string();
                let pol = match v.trim() {
                    "tiling" => crate::wm::LayoutPolicy::Tiling,
                    "floating" => crate::wm::LayoutPolicy::Floating,
                    other => {
                        warn!(line = n + 1, value = other, "unknown layout policy");
                        continue;
                    }
                };
                cfg.out_policy.insert(name, pol);
                continue;
            }
        }
        // `policy = tiling|floating` — all-monitors shorthand;
        // per-output policy.<name> lines override it.
        if let Some(rest) = line.strip_prefix("policy") {
            if let Some((_, v)) = rest.split_once('=') {
                let pol = match v.trim() {
                    "tiling" => crate::wm::LayoutPolicy::Tiling,
                    "floating" => crate::wm::LayoutPolicy::Floating,
                    other => {
                        warn!(line = n + 1, value = other, "unknown layout policy");
                        continue;
                    }
                };
                cfg.default_policy = Some(pol);
                info!(policy = ?pol, "default layout policy configured");
                continue;
            }
        }
        // `decoration = server|client|auto` — who draws window
        // frames: the compositor (SSD), the client (CSD), or
        // the negotiated result (auto, the default).
        if let Some(rest) = line.strip_prefix("decoration") {
            if let Some((_, v)) = rest.split_once('=') {
                cfg.deco = match v.trim() {
                    "server" => DecoPref::Server,
                    "client" => DecoPref::Client,
                    "auto" => DecoPref::Auto,
                    other => {
                        warn!(line = n + 1, value = other, "unknown decoration mode");
                        continue;
                    }
                };
                info!(deco = ?cfg.deco, "decoration policy configured");
                continue;
            }
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            warn!(line = n + 1, "bad binding line (no '=')");
            continue;
        };
        let Some((mods, key)) = parse_key(lhs.trim()) else {
            warn!(line = n + 1, key = lhs.trim(), "unknown key spec");
            continue;
        };
        cfg.bindings.insert((mods, key), rhs.trim().to_string());
    }
    info!(bindings = cfg.bindings.len(), ?p, "keybindings loaded");
    cfg
}
