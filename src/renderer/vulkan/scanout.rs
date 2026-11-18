// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Direct-scanout import — a DRM dumb buffer's dmabuf fd becomes
//! a Vulkan LINEAR image the render pass draws into directly.
//!
//! Import direction, deliberately: the DRM driver allocates
//! scanout memory in its own domain, so the framebuffer is
//! CRTC-compatible by construction. Exporting Vulkan host
//! memory the other way fails at `set_crtc` on drivers whose
//! display engine can't scan generic system memory (observed:
//! EINVAL on NVIDIA). Rendering into the imported BO removes
//! the readback→dumb CPU copy entirely — present is a bare
//! page-flip.

use super::context::VkContext;
use super::pipeline::Pipe;
use anyhow::{Context, Result};
use ash::vk;
use std::os::fd::RawFd;

/// A dumb BO imported as a render-target image plus the
/// framebuffer binding it to the scene render pass.
pub struct ScanImg {
    pub image: vk::Image,
    pub mem: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub fb: vk::Framebuffer,
}

impl ScanImg {
    /// Import `fd` (a LINEAR dmabuf for `w`×`h` BGRA at `pitch`
    /// bytes per row) as a color attachment target. Returns an
    /// error whenever the driver can't import this layout — the
    /// caller then stays on the CPU-copy present path.
    pub fn import(
        ctx: &VkContext,
        pipe: &Pipe,
        fd: RawFd,
        w: u32,
        h: u32,
        pitch: u32,
    ) -> Result<Self> {
        if !ctx.has_modifier_ext || !ctx.has_dma_buf_ext {
            anyhow::bail!("vulkan: modifier/dma-buf extension missing");
        }
        let dev = &ctx.device;
        // The image is declared against the dmabuf's real layout:
        // DRM_FORMAT_MOD_LINEAR, one plane, driver pitch.
        let mut ext_img = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let planes = [vk::SubresourceLayout::default()
            .offset(0)
            .row_pitch(pitch as u64)
            .size(0)
            .array_pitch(0)
            .depth_pitch(0)];
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(0) // DRM_FORMAT_MOD_LINEAR
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
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut ext_img)
                    .push_next(&mut mod_info),
                None,
            )
        }
        .context("vkCreateImage (scanout import)")?;
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        // Imported memory: the dmabuf fd fills the allocation —
        // any memory type the requirements allow is correct.
        if reqs.memory_type_bits == 0 {
            unsafe { dev.destroy_image(image, None) };
            anyhow::bail!("scanout import: no allowed memory types");
        }
        let ty = reqs.memory_type_bits.trailing_zeros();
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            unsafe { dev.destroy_image(image, None) };
            anyhow::bail!("dup(dmabuf fd) failed");
        }
        let mut imp = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup);
        let mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(ty)
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
            // A partial import must unwind completely — the
            // caller falls back to the CPU-copy path and this
            // state would leak otherwise.
            unsafe {
                dev.destroy_image(image, None);
                dev.free_memory(mem, None);
            }
            return Err(err).context("vkBindImageMemory (scanout import)");
        }
        let view = unsafe {
            dev.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::B8G8R8A8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        }
        .context("vkCreateImageView (scanout import)")?;
        let fb = unsafe {
            dev.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(pipe.render_pass)
                    .attachments(std::slice::from_ref(&view))
                    .width(w)
                    .height(h)
                    .layers(1),
                None,
            )
        }
        .context("vkCreateFramebuffer (scanout import)")?;
        Ok(Self {
            image,
            mem,
            view,
            fb,
        })
    }

    pub fn destroy(&self, dev: &ash::Device) {
        unsafe {
            dev.destroy_framebuffer(self.fb, None);
            dev.destroy_image_view(self.view, None);
            dev.destroy_image(self.image, None);
            dev.free_memory(self.mem, None);
        }
    }
}
