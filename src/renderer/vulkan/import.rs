// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client dmabuf import — a wl_buffer wrapping a LINEAR dmabuf
//! becomes a sampled VkImage bound to the client's own memory:
//! zero memcpy, zero staging, and the texture content tracks the
//! buffer live because it IS the buffer. This is what turns
//! fullscreen XWayland/EGL clients from an ~8 MB-per-frame CPU
//! readback over PCIe into a bare image import.
//!
//! Mirrors `scanout.rs`'s import of dumb BOs, with SAMPLED
//! instead of COLOR_ATTACHMENT usage and an alpha swizzle for
//! XRGB formats.

use super::context::VkContext;
use super::target::Tex;
use crate::compositor::shm::DmaRef;
use anyhow::{Context, Result};
use ash::vk;
use std::os::fd::IntoRawFd;

/// fourcc 'XR24' — XRGB needs the alpha channel forced opaque
/// in the view swizzle (the X bits are undefined).
const FMT_XRGB8888: u32 = 0x3432_5258;

/// Import a single-plane LINEAR dmabuf as a sampled texture.
/// The barrier UNDEFINED→GENERAL is recorded into `cmd` so the
/// first sampled draw sees a valid layout; afterwards the image
/// stays GENERAL for its whole life (same layout discipline as
/// pooled upload textures). Returns Err on any driver refusal —
/// the caller then draws via the mmap'd CPU fallback.
pub(crate) fn import_dma_tex(
    ctx: &VkContext,
    cmd: vk::CommandBuffer,
    dma: &DmaRef,
    w: u32,
    h: u32,
) -> Result<Tex> {
    if !ctx.has_modifier_ext || !ctx.has_dma_buf_ext {
        anyhow::bail!("vulkan: modifier/dma-buf extension missing");
    }
    let dev = &ctx.device;
    let mut ext_img = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // Explicit-modifier declaration: the client's modifier —
    // LINEAR for shm-exported buffers, a tiled driver modifier
    // (e.g. NVIDIA block-linear) for real GPU allocations.
    // dmabuf.rs already rejected anything outside the sampled
    // set, so the driver accepts this create.
    let planes = [vk::SubresourceLayout::default()
        .offset(dma.offset)
        .row_pitch(dma.stride as u64)
        .size(0)
        .array_pitch(0)
        .depth_pitch(0)];
    let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(dma.modifier)
        .plane_layouts(&planes);
    let image = unsafe {
        dev.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::B8G8R8A8_UNORM)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut ext_img)
                .push_next(&mut mod_info),
            None,
        )
    }
    .context("vkCreateImage (dmabuf import)")?;
    let reqs = unsafe { dev.get_image_memory_requirements(image) };
    if reqs.memory_type_bits == 0 {
        unsafe { dev.destroy_image(image, None) };
        anyhow::bail!("dmabuf import: no allowed memory types");
    }
    // Dedicated + imported: the fd's own memory backs the image.
    let ty = reqs.memory_type_bits.trailing_zeros();
    let dup = dma
        .fd
        .try_clone()
        .context("dup(dmabuf fd) failed")?
        .into_raw_fd();
    let mut imp = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(dup);
    let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mem = unsafe {
        dev.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(ty)
                .push_next(&mut ded)
                .push_next(&mut imp),
            None,
        )
    };
    let mem = match mem {
        Ok(m) => m,
        Err(err) => {
            unsafe {
                dev.destroy_image(image, None);
                libc::close(dup);
            }
            return Err(err).context("vkAllocateMemory (dmabuf import)");
        }
    };
    if let Err(err) = unsafe { dev.bind_image_memory(image, mem, 0) } {
        unsafe {
            dev.destroy_image(image, None);
            dev.free_memory(mem, None);
        }
        return Err(err).context("vkBindImageMemory (dmabuf import)");
    }
    // XRGB has no real alpha bits — pin the channel to 1 in the
    // view so sampling never multiplies garbage into the blend.
    let a_swizzle = if dma.format == FMT_XRGB8888 {
        vk::ComponentSwizzle::ONE
    } else {
        vk::ComponentSwizzle::IDENTITY
    };
    let view = unsafe {
        dev.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::B8G8R8A8_UNORM)
                .components(vk::ComponentMapping {
                    r: vk::ComponentSwizzle::IDENTITY,
                    g: vk::ComponentSwizzle::IDENTITY,
                    b: vk::ComponentSwizzle::IDENTITY,
                    a: a_swizzle,
                })
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )
    };
    let view = match view {
        Ok(v) => v,
        Err(err) => {
            unsafe {
                dev.destroy_image(image, None);
                dev.free_memory(mem, None);
            }
            return Err(err).context("vkCreateImageView (dmabuf import)");
        }
    };
    // UNDEFINED→GENERAL once at import — the texture is sampled
    // (never re-written by us) for the rest of its life.
    unsafe {
        dev.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })],
        );
    }
    Ok(Tex::imported(image, mem, view, w, h))
}
