// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pluggable render backends — the scene description lives in the
//! compositor; HOW pixels reach the scanout framebuffer is the
//! renderer's business and must stay swappable.
//!
//! Selection order (user requirement): Vulkan first on every GPU
//! that supports it, OpenGL ES as the compatibility fallback, and
//! the software CPU composite only when no GPU path exists at all.

pub mod cpu;
pub mod gles;
pub mod vulkan;

/// What a renderer backend needs from an output: the mode it's
/// compositing for plus the output's origin inside the global
/// scene space.
#[derive(Clone)]
pub struct OutputSpec {
    pub w: u32,
    pub h: u32,
    pub ox: i32,
    pub oy: i32,
    /// Connector name — per-output features (gamma ramps) key
    /// off it; "" on backends that have no real connector.
    pub name: String,
}

/// A dmabuf-exported frame plane — the Output imports each fd
/// into a DRM GEM handle and wraps it in a framebuffer, turning
/// presents into bare page-flips with no CPU copies.
pub struct DmaBufPlane {
    pub fd: std::os::fd::OwnedFd,
    pub w: u32,
    pub h: u32,
    /// Row pitch in bytes — LINEAR layout.
    pub pitch: u32,
}

/// One backend instance per output — GPU state is per-device and
/// per-target, never shared globally.
pub trait Backend {
    /// Render the current scene for this output and return the
    /// produced frame as packed BGRA rows (w*4). For zero-copy
    /// backends this slice is the host view of the target buffer;
    /// for the CPU path it's the scratch it composited into.
    ///
    /// `cover_clip` restricts the readback the submission must
    /// produce to the union of the output's queued damage (in
    /// output-local pixels, `None` = whole frame); `cover_seq`
    /// tags the newest queued damage-set sequence number this
    /// submission covers. Pipelined backends record both so the
    /// present path can tell which damage actually lives inside
    /// the pixels being returned.
    ///
    /// `tick` is a cooperative yield point: long CPU phases
    /// (surface uploads) must call it periodically so input
    /// drains and the cursor plane stay live mid-render. It may
    /// mutate comp (input dispatch adds damage for the NEXT
    /// frame — harmless mid-render).
    ///
    /// `blocked` is a bitmask over parities whose dmabuf
    /// framebuffer the CRTC scans now OR will scan once a
    /// pending page-flip lands — writing either mid-scan tears
    /// (and on some drivers wedges the submission outright), so
    /// a pipelined backend must render into a parity whose bit
    /// is clear. Bit i set = parity i untouchable; 0 = free.
    fn render(
        &mut self,
        comp: &mut crate::compositor::CompState,
        spec: OutputSpec,
        cover_clip: Option<&[(i32, i32, i32, i32)]>,
        cover_seq: u64,
        blocked: u8,
        tick: &mut dyn FnMut(&mut crate::compositor::CompState),
    ) -> anyhow::Result<&[u8]>;

    /// Import scanout planes — dmabuf descriptors of DRM-allocated
    /// dumb BOs, one per frame parity, index-matched to
    /// `last_retired`. A backend that can render into imported
    /// memory returns `true`; presents then flip fbs on those BOs
    /// instead of CPU-copying pixels into scanout. `false` keeps
    /// the copy path.
    fn attach_scanout(&mut self, planes: &[DmaBufPlane]) -> bool {
        let _ = planes;
        false
    }

    /// Parity index (into `dma_planes`) of the pixels the last
    /// `render`/`take_retired` returned — `None` until the first
    /// frame retires or when the backend isn't paritied.
    fn last_retired(&self) -> Option<usize> {
        None
    }

    /// Damage-set sequence bound of the pixels this backend can
    /// currently present — queued sets with a smaller or equal
    /// sequence number are guaranteed to exist inside the retired
    /// readback. Backends that always produce whole frames
    /// (cpu/gles) report MAX: every set is presentable.
    fn presentable_seq(&self) -> u64 {
        u64::MAX
    }

    /// Frames the returned pixels lag behind the scene — a
    /// pipelined backend submits asynchronously and hands back
    /// the previous frame's buffer, so its pixels miss the
    /// current damage set. The present path must only copy
    /// damage for frames ≤ `now - latency`.
    fn latency(&self) -> usize {
        0
    }

    /// Non-blocking present of the in-flight frame — pipelined
    /// backends strand their newest pixels GPU-side until the
    /// next render call; the event loop calls this every tick
    /// so the last frame still lands once its fence retires.
    /// Returns the retired buffer's pixel pointer exactly once.
    fn take_retired(&mut self) -> Option<*const u8> {
        None
    }
}

/// Try backends in preference order — Vulkan, then GLES, then CPU.
/// The card fd lets Vulkan pick the physical device that actually
/// owns this DRM node instead of gambling on enumeration order.
/// `ODIN_RENDERER=cpu|gles` forces a backend — the escape hatch
/// when a driver is broken enough to render but not to fail at
/// creation time.
pub fn create(card_fd: std::os::fd::RawFd, spec: OutputSpec) -> Box<dyn Backend> {
    match std::env::var("ODIN_RENDERER").as_deref() {
        Ok("cpu") => {
            tracing::info!("renderer: cpu (forced)");
            return Box::new(cpu::CpuBackend::new(spec.clone()));
        }
        Ok("gles") => {
            tracing::info!("renderer: gles (forced)");
            return match gles::GlesBackend::new(card_fd, spec.clone()) {
                Ok(gl) => Box::new(gl),
                Err(_) => Box::new(cpu::CpuBackend::new(spec.clone())),
            };
        }
        _ => {}
    }
    match vulkan::VulkanBackend::new(card_fd, spec.clone()) {
        Ok(vk) => {
            tracing::info!("renderer: vulkan");
            return Box::new(vk);
        }
        Err(err) => {
            tracing::info!(?err, "renderer: vulkan unavailable — trying gles");
        }
    }
    match gles::GlesBackend::new(card_fd, spec.clone()) {
        Ok(gl) => {
            tracing::info!("renderer: gles");
            return Box::new(gl);
        }
        Err(err) => {
            tracing::info!(?err, "renderer: gles unavailable — falling back to cpu");
        }
    }
    Box::new(cpu::CpuBackend::new(spec.clone()))
}
