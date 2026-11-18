// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! DMA upload path — client pixels travel through a
//! HOST_CACHED staging buffer into OPTIMAL, DEVICE_LOCAL
//! textures via `vkCmdCopyBufferToImage` on the graphics queue.
//! Compared to memcpy'ing into WC-mapped LINEAR sysmem images
//! (~0.5 GB/s writes, and the GPU re-reads every sampled byte
//! over PCIe each frame), this mirrors what desktop drivers do
//! internally: fast cached CPU writes, one DMA move into VRAM,
//! then sampling from local memory. Kept in its own file next to
//! the linear fallback in `upload.rs`.

use super::context;
use super::target::{HostBuf, Tex};
use anyhow::Result;
use ash::vk;

/// Per-parity upload staging — a frame writes into it while
/// recording commands and the parity's own submission reads it
/// back, which is what makes the CPU writes race-free: the
/// parity is only re-recorded after its fence retired.
pub(crate) struct Staging {
    buf: HostBuf,
    /// Bump cursor — rewound at the start of each render pass.
    off: u64,
    /// Buffers swapped out by mid-frame growth — still
    /// referenced by copy commands recorded this frame, so they
    /// stay mapped until the parity's fence retires.
    retire: Vec<HostBuf>,
    /// Upload accounting for the present-stats line — bytes
    /// pushed into staging plus how many full vs patch uploads
    /// the frame performed. `render()` resets it per frame.
    pub up_bytes: u64,
    /// Full-upload count this frame.
    pub up_full: u32,
    /// Patch-upload count this frame.
    pub up_patch: u32,
    /// Largest single upload this frame — (bytes, w, h). Names
    /// the dominant surface in the stats line: constant
    /// multi-MB frames usually trace back to one big client.
    pub up_big: (u64, i32, i32),
}

/// Staging allocation — HOST_CACHED makes the row memcpy write
/// at system-memory speed; a driver without a cached host type
/// falls back to plain coherent sysmem (still correct, just the
/// old write speed).
fn alloc_stage(ctx: &context::VkContext, cap: usize) -> Result<HostBuf> {
    HostBuf::alloc(
        ctx,
        cap,
        vk::BufferUsageFlags::TRANSFER_SRC,
        vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT
            | vk::MemoryPropertyFlags::HOST_CACHED,
    )
    .or_else(|_| {
        HostBuf::alloc(
            ctx,
            cap,
            vk::BufferUsageFlags::TRANSFER_SRC,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
    })
}

impl Staging {
    pub(crate) fn new(ctx: &context::VkContext, cap: usize) -> Result<Self> {
        Ok(Self {
            buf: alloc_stage(ctx, cap)?,
            off: 0,
            retire: Vec::new(),
            up_bytes: 0,
            up_full: 0,
            up_patch: 0,
            up_big: (0, 0, 0),
        })
    }

    /// Reset the bump cursor — legal because the owning parity's
    /// previous submission provably retired its fence already.
    pub(crate) fn rewind(&mut self) {
        self.off = 0;
    }

    /// Free replaced buffers once the parity's fence signaled —
    /// called from the retire sweep.
    pub(crate) fn drain_retired(&mut self, dev: &ash::Device) {
        for b in self.retire.drain(..) {
            b.destroy(dev);
        }
    }

    /// Reserve `need` bytes — returns the current buffer handle,
    /// the base offset, and the write pointer for row packing.
    /// Grows the buffer when the request doesn't fit; the
    /// replaced buffer joins `retire` because copies already
    /// recorded against it execute later.
    fn take(&mut self, ctx: &context::VkContext, need: u64) -> Result<(vk::Buffer, u64, *mut u8)> {
        if self.off + need > self.buf.size as u64 {
            let cap = ((self.buf.size as u64) * 2).max(need) as usize;
            let old = std::mem::replace(&mut self.buf, alloc_stage(ctx, cap)?);
            self.retire.push(old);
            self.off = 0;
        }
        let off = self.off;
        self.off += need;
        Ok((self.buf.buf, off, unsafe { self.buf.ptr.add(off as usize) }))
    }
}

/// Shared subresource range — single 2D color layer.
fn range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// Host-writes→transfer visibility plus the image's move into
/// TRANSFER_DST — `old` is UNDEFINED for fresh textures and
/// GENERAL for pooled/patched ones.
fn xfer_begin(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
) {
    unsafe {
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(if old == vk::ImageLayout::GENERAL {
                    vk::AccessFlags::SHADER_READ
                } else {
                    vk::AccessFlags::empty()
                })
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(old)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(range())],
        );
    }
}

/// Copy `regions` `(buf_off, x, y, w, h)` from `buf` into the
/// image — tightly packed rows (`row_length = 0` = w).
fn xfer_regions(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    buf: vk::Buffer,
    regions: &[(u64, i32, i32, i32, i32)],
) {
    let regs: Vec<vk::BufferImageCopy> = regions
        .iter()
        .map(|&(off, x, y, w, h)| {
            vk::BufferImageCopy::default()
                .buffer_offset(off)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x, y, z: 0 })
                .image_extent(vk::Extent3D {
                    width: w.max(0) as u32,
                    height: h.max(0) as u32,
                    depth: 1,
                })
        })
        .collect();
    if regs.is_empty() {
        return;
    }
    unsafe {
        ctx.device.cmd_copy_buffer_to_image(
            cmd,
            buf,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &regs,
        );
    }
}

/// TRANSFER_DST → GENERAL — sampling keeps the same layout the
/// descriptor path already binds.
fn xfer_end(ctx: &context::VkContext, cmd: vk::CommandBuffer, image: vk::Image) {
    unsafe {
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(image)
                .subresource_range(range())],
        );
    }
}

/// Full-surface DMA upload — rows pack tightly into staging,
/// one copy command moves them into a fresh OPTIMAL texture.
/// Same pool discipline as the linear path: recycled textures
/// arrive in GENERAL, fresh ones in UNDEFINED.
pub(crate) fn upload_tex_dma(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    st: &mut Staging,
    src: *const u8,
    stride: usize,
    sw: i32,
    sh: i32,
    pool: &mut std::collections::HashMap<(u32, u32), Vec<Tex>>,
) -> Result<Tex> {
    let dims = (sw.max(0) as u32, sh.max(0) as u32);
    let (tex, old) = match pool.get_mut(&dims).and_then(|v| v.pop()) {
        Some(t) => (t, vk::ImageLayout::GENERAL),
        None => (
            Tex::new(
                ctx,
                dims.0,
                dims.1,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?,
            vk::ImageLayout::UNDEFINED,
        ),
    };
    let row = sw.max(0) as usize * 4;
    let need = row as u64 * sh.max(0) as u64;
    let (buf, off, dst) = st.take(ctx, need)?;
    st.up_bytes += need;
    st.up_full += 1;
    if need > st.up_big.0 {
        st.up_big = (need, sw, sh);
    }
    unsafe {
        for y in 0..sh.max(0) as usize {
            std::ptr::copy_nonoverlapping(src.add(y * stride), dst.add(y * row), row);
        }
        xfer_begin(ctx, cmd, tex.image, old);
        xfer_regions(ctx, cmd, tex.image, buf, &[(off, 0, 0, sw, sh)]);
        xfer_end(ctx, cmd, tex.image);
    }
    Ok(tex)
}

/// Damage-clipped DMA patch — each rect's rows pack into
/// staging and copy straight into their image offset. The one
/// complication vs the memcpy variant is mid-list buffer
/// growth: `take` may swap the staging buffer, so copy commands
/// flush whenever the buffer handle changes.
pub(crate) fn upload_rects_dma(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    st: &mut Staging,
    tex: &Tex,
    src: *const u8,
    stride: usize,
    rects: &[(i32, i32, i32, i32)],
) -> Result<()> {
    let (tw, th) = (tex.w as i32, tex.h as i32);
    let mut regs: Vec<(u64, i32, i32, i32, i32)> = Vec::with_capacity(rects.len());
    let mut cur = vk::Buffer::null();
    st.up_patch += 1;
    unsafe {
        xfer_begin(ctx, cmd, tex.image, vk::ImageLayout::GENERAL);
        for &(x, y, w, h) in rects {
            let (x0, y0) = (x.max(0), y.max(0));
            let (x1, y1) = ((x + w).min(tw), (y + h).min(th));
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let (rw, rh) = (x1 - x0, y1 - y0);
            let row = rw as usize * 4;
            let (buf, off, dst) = st.take(ctx, row as u64 * rh as u64)?;
            if buf != cur && !regs.is_empty() {
                xfer_regions(ctx, cmd, tex.image, cur, &regs);
                regs.clear();
            }
            cur = buf;
            for yy in y0..y1 {
                let s = yy as usize * stride + x0 as usize * 4;
                let d = (yy - y0) as usize * row;
                std::ptr::copy_nonoverlapping(src.add(s), dst.add(d), row);
            }
            let rb = row as u64 * rh as u64;
            st.up_bytes += rb;
            if rb > st.up_big.0 {
                st.up_big = (rb, rw, rh);
            }
            regs.push((off, x0, y0, rw, rh));
        }
        xfer_regions(ctx, cmd, tex.image, cur, &regs);
        xfer_end(ctx, cmd, tex.image);
    }
    Ok(())
}
