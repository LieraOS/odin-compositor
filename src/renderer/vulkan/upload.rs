// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-frame resource staging — the helpers `render()` calls to
//! get client pixels into GPU textures and to bind them. Kept
//! apart so the frame path stays readable.

use super::context;
use super::pipeline::Pipe;
use super::target::Tex;
use anyhow::{Context, Result};
use ash::vk;

/// Allocate a descriptor set for `view` and write the
/// image+sampler bindings — one per draw, retired with the pool
/// reset each frame.
pub(crate) fn alloc_ds(
    dev: &ash::Device,
    pipe: &Pipe,
    pool: vk::DescriptorPool,
    view: vk::ImageView,
) -> Result<vk::DescriptorSet> {
    let ds = unsafe {
        dev.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&[pipe.ds_layout]),
        )?[0]
    };
    let img = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let smp = [vk::DescriptorImageInfo::default().sampler(pipe.sampler)];
    unsafe {
        dev.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(ds)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&img),
                vk::WriteDescriptorSet::default()
                    .dst_set(ds)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&smp),
            ],
            &[],
        );
    }
    Ok(ds)
}

/// Per-buffer upload into a host-linear texture — memcpy rows plus
/// the host-write→shader-read barrier recorded into `cmd`. Cached
/// by wl_buffer id: only an actual client commit triggers a new
/// upload, pointer-motion frames just rebind the cached texture.
///
/// `pool` recycles textures whose wl_buffer died, keyed by size —
/// clients that commit a fresh buffer every frame (animated menus)
/// would otherwise pay a vkImage+memory+view allocation per frame.
pub(crate) fn upload_tex(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    src: *const u8,
    stride: usize,
    sw: i32,
    sh: i32,
    pool: &mut std::collections::HashMap<(u32, u32), Vec<Tex>>,
    arena: Option<&mut super::target::Arena>,
) -> Result<Tex> {
    let dims = (sw as u32, sh as u32);
    // A recycled texture already sits in GENERAL — only the very
    // first upload on a fresh image starts from UNDEFINED.
    let fresh = |arena: Option<&mut super::target::Arena>| -> Result<Tex> {
        match arena {
            Some(ar) => Tex::new_host_in(ctx, ar, dims.0, dims.1, vk::ImageUsageFlags::SAMPLED),
            None => Tex::new_host(ctx, dims.0, dims.1, vk::ImageUsageFlags::SAMPLED),
        }
    };
    let (tex, old_layout) = match pool.get_mut(&dims).and_then(|v| v.pop()) {
        Some(t) => (t, vk::ImageLayout::GENERAL),
        None => (fresh(arena)?, vk::ImageLayout::UNDEFINED),
    };
    unsafe {
        for y in 0..sh as usize {
            std::ptr::copy_nonoverlapping(
                src.add(y * stride),
                tex.ptr.add(y * tex.pitch),
                sw as usize * 4,
            );
        }
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(tex.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })],
        );
    }
    Ok(tex)
}

/// Damage-clipped in-place upload — the cached texture keeps its
/// previous content; only the rows inside the client's damage
/// rects get rewritten. Rects are buffer-space and clamped to
/// the texture extent so an out-of-bounds rect can never read
/// past the guaranteed `(h-1)*stride + w*4` source bound. Used
/// when the same wl_buffer id committed new content — a
/// 1920×1200 surface repainting a 40×20 clock then uploads a
/// few KB instead of 9 MB.
pub(crate) fn upload_rects(
    ctx: &context::VkContext,
    cmd: vk::CommandBuffer,
    tex: &Tex,
    src: *const u8,
    stride: usize,
    rects: &[(i32, i32, i32, i32)],
) {
    let (tw, th) = (tex.w as i32, tex.h as i32);
    unsafe {
        for &(x, y, w, h) in rects {
            let (x0, y0) = (x.max(0), y.max(0));
            let (x1, y1) = ((x + w).min(tw), (y + h).min(th));
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let row = (x1 - x0) as usize * 4;
            for yy in y0..y1 {
                let s = yy as usize * stride + x0 as usize * 4;
                let d = yy as usize * tex.pitch + x0 as usize * 4;
                std::ptr::copy_nonoverlapping(src.add(s), tex.ptr.add(d), row);
            }
        }
        // The texture already sits in GENERAL — repeat the
        // host-write→shader-read barrier for the new bytes.
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(tex.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })],
        );
    }
}

/// One-shot upload — host-visible linear image, memcpy rows, and a
/// one-time transition into GENERAL where it stays for sampling.
pub(crate) fn upload_once(ctx: &context::VkContext, w: u32, h: u32, bgra: &[u8]) -> Result<Tex> {
    let tex = Tex::new_host(ctx, w, h, vk::ImageUsageFlags::SAMPLED)?;
    unsafe {
        for y in 0..h as usize {
            std::ptr::copy_nonoverlapping(
                bgra.as_ptr().add(y * w as usize * 4),
                tex.ptr.add(y * tex.pitch),
                w as usize * 4,
            );
        }
    }
    let dev = &ctx.device;
    let cmd = unsafe {
        dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(ctx.cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .context("alloc upload cmd")?[0];
    unsafe {
        dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        // Single transition: UNDEFINED → GENERAL, host writes made
        // visible to the shader. The image stays in GENERAL for
        // its whole lifetime — no per-frame transitions needed.
        dev.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(tex.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })],
        );
        dev.end_command_buffer(cmd)?;
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        dev.queue_submit(ctx.queue, &submit, vk::Fence::null())?;
        dev.queue_wait_idle(ctx.queue)?;
        dev.free_command_buffers(ctx.cmd_pool, &cmds);
    }
    Ok(tex)
}
