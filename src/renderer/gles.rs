// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! GLES fallback — GBM + EGL textured-quad composite. Placeholder
//! until the Vulkan path lands: selection order stays
//! Vulkan → GLES → CPU and this slot simply reports unavailable
//! so the cascade reaches the CPU backend today.

use super::{Backend, OutputSpec};

pub struct GlesBackend;

impl GlesBackend {
    pub fn new(_card_fd: std::os::fd::RawFd, _spec: OutputSpec) -> anyhow::Result<Self> {
        anyhow::bail!("gles backend not implemented yet")
    }
}

impl Backend for GlesBackend {
    fn render(
        &mut self,
        _comp: &mut crate::compositor::CompState,
        _spec: OutputSpec,
        _cover_clip: Option<&[(i32, i32, i32, i32)]>,
        _cover_seq: u64,
        _blocked: u8,
        _tick: &mut dyn FnMut(&mut crate::compositor::CompState),
    ) -> anyhow::Result<&[u8]> {
        anyhow::bail!("gles backend not implemented yet")
    }
}
