// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Vulkan backend — the primary renderer. Compositing is a single
//! render pass of textured quads (one per surface, cursor last),
//! read back through a host-visible buffer until the DMABUF
//! modifier path replaces the copy with a direct framebuffer
//! import.

pub(crate) mod context;
pub(crate) mod drain;
pub(crate) mod draw;
pub(crate) mod import;
mod pipeline;
pub(crate) mod render;
mod scanout;
pub(crate) mod selftest;
pub(crate) mod submit_test;
mod target;
mod upload;
mod upload_dma;

use super::{Backend, OutputSpec};
use anyhow::Result;
use ash::vk;
use target::{Target, Tex};

/// `--vk-probe` helper — initializes the device context and returns
/// a summary so physical-device matching can be verified on real
/// hardware without a session.
pub fn context_probe(card_fd: std::os::fd::RawFd) -> Result<String> {
    let ctx = context::VkContext::new(card_fd)?;
    Ok(format!(
        "vulkan context up — drm-modifier-ext={}",
        ctx.has_modifier_ext
    ))
}

/// Readback parity count — three slots so direct scanout
/// always has a buffer that is neither on the CRTC nor the
/// just-retired frame about to present.
pub(crate) const PARITIES: usize = 3;

/// An imported dmabuf texture plus the source fd it was built
/// from — the fd distinguishes a recycled wl_buffer id that
/// happens to share dims from the buffer actually still alive.
pub(crate) struct ImpTex {
    pub tex: Tex,
    pub fd: std::os::fd::RawFd,
}

pub struct VulkanBackend {
    pub(crate) ctx: context::VkContext,
    pub(crate) pipe: pipeline::Pipe,
    pub(crate) target: Target,
    /// Compositor fallback arrow as a texture — uploaded once.
    pub(crate) arrow: Tex,
    /// Compute drain — graphics-engine readback, no copy engine.
    pub(crate) drain: drain::Drain,
    /// Drain descriptor sets — one per readback parity, each
    /// pre-bound to its own destination buffer.
    pub(crate) drain_ds: [vk::DescriptorSet; PARITIES],
    /// Readback parity — `cur` is the buffer the GPU drains into
    /// this frame; the CPU is handed `1 - cur` (the previous
    /// frame's completed buffer) so submit never blocks the loop.
    pub(crate) cur: usize,
    /// The very first frame waits on its own submit so the pixels
    /// returned are real — every later frame presents the
    /// previous parity.
    pub(crate) first_present: bool,
    /// Parities still executing on the GPU — set at submit,
    /// cleared when the parity's fence signals. Up to two frames
    /// may be in flight; the CPU only ever waits on the parity
    /// it is about to re-record, never on the newest submission.
    pub(crate) in_flight: [bool; PARITIES],
    /// Damage-set sequence each parity's last submission covers —
    /// the readback clip is the union of every queued set, so
    /// `cover_seq[i]` bounds what parity `i`'s pixels contain.
    pub(crate) cover_seq: [u64; PARITIES],
    /// Sequence bound of the most recently retired readback —
    /// queued damage sets ≤ this are provably inside the pixels
    /// the present path may copy.
    pub(crate) presentable: u64,
    /// wl_buffer → texture cache — uploads happen on commit only,
    /// not per frame.
    /// Texture cache — buffer id → (commit generation, texture).
    /// The generation invalidates on rewrite: a client damaging
    /// a wl_buffer it already attached must re-upload, otherwise
    /// the screen shows stale pixels forever.
    pub(crate) tex_cache: std::collections::HashMap<wayland_server::backend::ObjectId, (u64, Tex)>,
    /// 1×1 solid-color textures for compositor-owned fill slots
    /// (SSD chrome) — keyed by the packed BGRA value so every
    /// frame color is uploaded exactly once.
    pub(crate) fill_texs: std::collections::HashMap<u32, Tex>,
    /// Dead-buffer texture recycling, keyed by size — keeps
    /// commit-per-frame clients (animated menus) off the
    /// vkAllocateMemory path.
    pub(crate) tex_pool: std::collections::HashMap<(u32, u32), Vec<Tex>>,
    /// Imported dmabuf textures, keyed by wl_buffer id — bound
    /// to the client's own memory, so they are NEVER pooled and
    /// NEVER re-imported while the buffer lives: the content
    /// follows the dmabuf without a single byte of upload.
    pub(crate) imp_cache: std::collections::HashMap<wayland_server::backend::ObjectId, ImpTex>,
    /// wl_buffer ids whose dmabuf import already failed — one
    /// vkCreateImage refusal shouldn't cost a driver call every
    /// single frame; they draw via the mmap'd fallback instead.
    pub(crate) imp_bad: std::collections::HashSet<wayland_server::backend::ObjectId>,
    /// Textures replaced mid-frame — a previously submitted
    /// command buffer can still sample them through its
    /// descriptor set, so they return to the pool (or die) only
    /// once every non-current parity has retired. Freeing one
    /// mid-flight is a use-after-free on the GPU — NVIDIA
    /// answers that with VK_ERROR_DEVICE_LOST.
    pub(crate) tex_retire: Vec<Tex>,
    /// Same retirement for imported dmabuf textures — never
    /// pooled (the memory is one specific dmabuf), so they are
    /// destroyed outright at the next retirement sweep.
    pub(crate) imp_retire: Vec<Tex>,
    /// Themed cursor texture + its hotspot — persistent host
    /// image the frame path rewrites when a cursor-shape is set.
    pub(crate) cursor_tex: Option<(Tex, i32, i32)>,
    /// Parity of the pixels the last `render`/`take_retired`
    /// handed to the present path — the index matching the
    /// `dma_planes`/`scan_fbs` arrays for direct scanout.
    pub(crate) last_out: Option<usize>,
    /// Imported scanout targets — dumb-BO dmabufs bound as
    /// render-pass framebuffers, one per parity. `Some` means
    /// the render pass draws straight into scanout memory and
    /// the drain+readback is skipped entirely.
    pub(crate) scan: Option<[scanout::ScanImg; PARITIES]>,
    /// Shared host-memory arena — surface upload textures bind
    /// slots out of one permanent allocation instead of paying
    /// a vkAllocateMemory+vkMapMemory pair per texture. `None`
    /// only when the arena allocation itself failed.
    pub(crate) arena: Option<target::Arena>,
    /// Per-parity upload staging for the DMA path (cached
    /// buffer → `vkCmdCopyBufferToImage` → OPTIMAL/VRAM texture).
    /// Empty means the linear-sysmem fallback stays in use —
    /// `ODIN_LINEAR_UPLOAD=1` forces that for driver
    /// workarounds.
    pub(crate) staging: Vec<upload_dma::Staging>,
}

impl VulkanBackend {
    pub fn new(card_fd: std::os::fd::RawFd, spec: OutputSpec) -> Result<Self> {
        let ctx = context::VkContext::new(card_fd)?;
        let pipe = pipeline::Pipe::new(&ctx.device, vk::Format::B8G8R8A8_UNORM)?;
        let target = Target::new(&ctx, &pipe, spec.w, spec.h)?;
        // Build the fallback arrow once — same two-tone pixels the
        // CPU rasterizer draws, as a BGRA texture.
        let mut px = vec![0u8; 12 * 19 * 4];
        {
            let mut buf = [0u8; 12 * 19 * 4];
            crate::cursor::draw(&mut buf, 12, 19, 0, 0);
            px.copy_from_slice(&buf);
        }
        let arrow = upload::upload_once(&ctx, 12, 19, &px)?;
        let drain = drain::Drain::new(&ctx)?;
        let mut drain_ds = [vk::DescriptorSet::null(); PARITIES];
        for (i, ds) in drain_ds.iter_mut().enumerate() {
            *ds = drain.alloc_ds(
                &ctx.device,
                pipe.sampler,
                target.color.view,
                target.readbacks[i].buf,
                (spec.w * spec.h * 4) as u64,
            )?;
        }
        // 256 MiB of mapped host memory covers ~27 full-HD
        // textures before falling back to dedicated allocs —
        // try progressively smaller arenas on tight systems.
        let arena = target::Arena::new(&ctx, 256 << 20)
            .or_else(|_| target::Arena::new(&ctx, 64 << 20))
            .ok();
        // DMA upload staging — one cached staging buffer per
        // parity so the frame's own submission owns its bytes.
        // `ODIN_LINEAR_UPLOAD=1` keeps the memcpy-into-linear
        // path as a driver-workaround switch; a failed alloc
        // degrades to the same fallback.
        let staging = if std::env::var_os("ODIN_LINEAR_UPLOAD").is_some() {
            Vec::new()
        } else {
            (0..PARITIES)
                .map(|_| upload_dma::Staging::new(&ctx, 16 << 20))
                .collect::<Result<Vec<_>>>()
                .unwrap_or_default()
        };
        tracing::info!(
            arena = arena.is_some(),
            staging = staging.len(),
            "vulkan backend ready"
        );
        Ok(Self {
            ctx,
            pipe,
            target,
            arrow,
            drain,
            drain_ds,
            cur: 0,
            first_present: true,
            in_flight: [false; PARITIES],
            cover_seq: [0; PARITIES],
            presentable: 0,
            tex_cache: std::collections::HashMap::new(),
            fill_texs: std::collections::HashMap::new(),
            tex_pool: std::collections::HashMap::new(),
            imp_cache: std::collections::HashMap::new(),
            imp_bad: std::collections::HashSet::new(),
            tex_retire: Vec::new(),
            imp_retire: Vec::new(),
            cursor_tex: None,
            last_out: None,
            scan: None,
            arena,
            staging,
        })
    }
}

impl Drop for VulkanBackend {
    fn drop(&mut self) {
        unsafe {
            let _ = self.ctx.device.device_wait_idle();
        }
        if let Some(scan) = &self.scan {
            for img in scan {
                img.destroy(&self.ctx.device);
            }
        }
        self.drain.destroy(&self.ctx.device);
        self.arrow.destroy(&self.ctx.device, None);
        // Surface textures and the pool are raw vk handles — no
        // Drop impl — so they die explicitly here, not via map
        // eviction (which recycles into the pool instead).
        for (_, tex) in self.tex_cache.values() {
            tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        for tex in self.tex_pool.values().flatten() {
            tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        for it in self.imp_cache.values() {
            it.tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        // Deferred retire lists — device_wait_idle above already
        // proved the queue empty, so these die like every cache.
        for tex in self.tex_retire.drain(..) {
            tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        for tex in self.imp_retire.drain(..) {
            tex.destroy(&self.ctx.device, None);
        }
        for tex in self.fill_texs.values() {
            tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        if let Some((tex, _, _)) = &self.cursor_tex {
            tex.destroy(&self.ctx.device, self.arena.as_mut());
        }
        if let Some(ar) = &self.arena {
            unsafe {
                self.ctx.device.unmap_memory(ar.mem);
                self.ctx.device.free_memory(ar.mem, None);
            }
        }
        self.target.destroy(&self.ctx.device);
        unsafe {
            self.ctx.device.destroy_sampler(self.pipe.sampler, None);
            self.ctx.device.destroy_pipeline(self.pipe.pipeline, None);
            self.ctx
                .device
                .destroy_pipeline_layout(self.pipe.layout, None);
            self.ctx
                .device
                .destroy_descriptor_pool(self.pipe.ds_pool, None);
            self.ctx
                .device
                .destroy_descriptor_set_layout(self.pipe.ds_layout, None);
            self.ctx
                .device
                .destroy_render_pass(self.pipe.render_pass, None);
        }
    }
}
