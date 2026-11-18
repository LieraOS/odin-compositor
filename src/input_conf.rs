// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Input-device configuration — `~/.config/odin/input.conf`.
//! Without it libinput runs its compiled-in defaults
//! and pointer feel never matches the user's tuned session
//! (a high-DPI mouse at default speed reads as "unaccelerated
//! and slow").
//!
//! Format — one `class.key = value` per line, `#` comments:
//!   pointer.accel_profile   = adaptive | flat
//!   pointer.accel_speed     = -1.0 .. 1.0
//!   pointer.natural_scroll  = 0 | 1
//!   pointer.left_handed     = 0 | 1
//!   touchpad.tap            = 0 | 1
//!   touchpad.tap_drag       = 0 | 1
//!   touchpad.natural_scroll = 0 | 1
//!   touchpad.dwt            = 0 | 1   (disable-while-typing)
//!   touchpad.accel_profile  = adaptive | flat
//!   touchpad.accel_speed    = -1.0 .. 1.0
//!   touchpad.scroll_method  = twofinger | edge | none
//!   touchpad.left_handed    = 0 | 1
//!
//! Only keys present in the file are applied — absent keys keep
//! libinput's defaults, so a partial config never resets what
//! the user didn't touch.

use input::{AccelProfile, Device, DeviceCapability, ScrollMethod};
use tracing::{debug, warn};

/// Per-class settings — Option per key: `None` = untouched,
/// the device keeps whatever libinput/udev decided.
#[derive(Debug, Default)]
pub struct DeviceConf {
    pub accel_profile: Option<AccelProfile>,
    pub accel_speed: Option<f64>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    // Touchpad-only keys — ignored on plain pointer devices.
    pub tap: Option<bool>,
    pub tap_drag: Option<bool>,
    pub dwt: Option<bool>,
    pub scroll_method: Option<ScrollMethod>,
}

/// The whole input.conf content, split by device class.
#[derive(Debug, Default)]
pub struct InputConf {
    pub pointer: DeviceConf,
    pub touchpad: DeviceConf,
}

/// Parse a boolean-ish config value — accepts 1/0, true/false,
/// yes/no so handwritten configs can't fail on spelling.
fn as_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse an accel profile name.
fn as_profile(v: &str) -> Option<AccelProfile> {
    match v.trim().to_ascii_lowercase().as_str() {
        "flat" => Some(AccelProfile::Flat),
        "adaptive" => Some(AccelProfile::Adaptive),
        _ => None,
    }
}

/// Parse a scroll-method name.
fn as_scroll(v: &str) -> Option<ScrollMethod> {
    match v.trim().to_ascii_lowercase().as_str() {
        "twofinger" | "two_finger" => Some(ScrollMethod::TwoFinger),
        "edge" => Some(ScrollMethod::Edge),
        "button" | "onbuttondown" | "on_button_down" => Some(ScrollMethod::OnButtonDown),
        "none" => Some(ScrollMethod::NoScroll),
        _ => None,
    }
}

impl InputConf {
    /// Load `~/.config/odin/input.conf` — missing file means
    /// "all libinput defaults", unknown keys are logged and
    /// skipped so a typo never kills the compositor.
    pub fn load() -> Self {
        let mut conf = InputConf::default();
        let p = crate::config::path().with_file_name("input.conf");
        let Ok(text) = std::fs::read_to_string(&p) else {
            return conf;
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                warn!(line = n + 1, "input.conf: ignoring malformed line");
                continue;
            };
            let (key, val) = (key.trim(), val.trim());
            let Some((class, k)) = key.split_once('.') else {
                warn!(line = n + 1, key, "input.conf: key needs a class prefix");
                continue;
            };
            let dev = match class {
                "pointer" => &mut conf.pointer,
                "touchpad" => &mut conf.touchpad,
                _ => {
                    warn!(line = n + 1, class, "input.conf: unknown device class");
                    continue;
                }
            };
            match k {
                "accel_profile" => dev.accel_profile = as_profile(val),
                "accel_speed" => dev.accel_speed = val.parse::<f64>().ok(),
                "natural_scroll" => dev.natural_scroll = as_bool(val),
                "left_handed" => dev.left_handed = as_bool(val),
                "tap" => dev.tap = as_bool(val),
                "tap_drag" => dev.tap_drag = as_bool(val),
                "dwt" => dev.dwt = as_bool(val),
                "scroll_method" => dev.scroll_method = as_scroll(val),
                _ => warn!(line = n + 1, key, "input.conf: unknown key"),
            }
        }
        conf
    }
}

/// Push one class block onto a device — every setter is
/// capability-checked by libinput itself: a plain mouse can't
/// take tap config and libinput reports the failure without
/// harm, so we fire-and-log instead of pre-detecting.
fn apply_class(dev: &mut Device, c: &DeviceConf, name: &str) {
    if let Some(p) = c.accel_profile {
        debug!(?p, name, "input: accel profile");
        let _ = dev.config_accel_set_profile(p);
    }
    if let Some(s) = c.accel_speed {
        debug!(s, name, "input: accel speed");
        let _ = dev.config_accel_set_speed(s);
    }
    if let Some(b) = c.natural_scroll {
        debug!(b, name, "input: natural scroll");
        let _ = dev.config_scroll_set_natural_scroll_enabled(b);
    }
    if let Some(b) = c.left_handed {
        debug!(b, name, "input: left handed");
        let _ = dev.config_left_handed_set(b);
    }
    if let Some(b) = c.tap {
        debug!(b, name, "input: tap to click");
        let _ = dev.config_tap_set_enabled(b);
    }
    if let Some(b) = c.tap_drag {
        debug!(b, name, "input: tap drag");
        let _ = dev.config_tap_set_drag_enabled(b);
    }
    if let Some(b) = c.dwt {
        debug!(b, name, "input: disable while typing");
        let _ = dev.config_dwt_set_enabled(b);
    }
    if let Some(m) = c.scroll_method {
        debug!(?m, name, "input: scroll method");
        let _ = dev.config_scroll_set_method(m);
    }
}

/// Apply the configured device classes — a device is a
/// "touchpad" for our purposes when it offers tap config
/// (mice and pointing sticks don't), plus the pointer block
/// which every Pointer-capable device takes.
pub fn apply_device(dev: &mut Device, conf: &InputConf) {
    if !dev.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let name = dev.name().to_string();
    apply_class(dev, &conf.pointer, &name);
    // Touchpad detection via the Gesture capability — libinput
    // tags touchpads with swipe/pinch support, mice never get
    // it; cleaner than matching on device names.
    if dev.has_capability(DeviceCapability::Gesture) {
        apply_class(dev, &conf.touchpad, &name);
    }
}
