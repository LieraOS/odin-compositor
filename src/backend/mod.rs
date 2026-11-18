// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Backend layer — session hardware access (drm, input).
pub(crate) mod direct;
pub mod drm;
pub mod eventloop;
pub mod gpu;
pub mod hotplug;
pub mod hwcursor;
pub mod input;
pub mod nested;
pub mod nested_dispatch;
pub mod nested_events;
pub mod nested_frame;
pub mod nested_selftest;
pub(crate) mod outcfg;
pub(crate) mod output_new;
pub(crate) mod present;
pub mod render;
pub mod teardown;
