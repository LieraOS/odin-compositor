// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Render pipeline — one render pass, one textured-quad pipeline,
//! premultiplied src-over blending (wayland shm buffers are
//! premultiplied). Descriptor sets are allocated per draw from a
//! reset pool — simple and correct before any caching layer.

use anyhow::{Context, Result};
use ash::vk;

/// The objects a draw needs — owned by the backend, shared across
/// frames and surfaces.
pub struct Pipe {
    pub render_pass: vk::RenderPass,
    pub ds_layout: vk::DescriptorSetLayout,
    pub ds_pool: vk::DescriptorPool,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub sampler: vk::Sampler,
}

impl Pipe {
    pub fn new(device: &ash::Device, format: vk::Format) -> Result<Self> {
        // One attachment, cleared once per frame.
        let attach = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::GENERAL)
            .final_layout(vk::ImageLayout::GENERAL)];
        let aref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let sub = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&aref)];
        let rp = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attach)
                    .subpasses(&sub),
                None,
            )
        }
        .context("vkCreateRenderPass")?;

        // binding 0 = sampled image, binding 1 = sampler.
        let binds = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
                None,
            )
        }
        .context("vkCreateDescriptorSetLayout")?;
        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(256),
        ];
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                    .max_sets(256)
                    .pool_sizes(&sizes),
                None,
            )
        }
        .context("vkCreateDescriptorPool")?;

        // Push constants: ndc rect + uv rect = 32 bytes for the
        // vertex stage; the alpha multiplier rides offset 32 on
        // the fragment stage (shared block, per-stage overlay).
        let pcr = [
            vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX)
                .offset(0)
                .size(32),
            vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .offset(32)
                .size(4),
        ];
        let layouts = [ds_layout];
        let layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&pcr),
                None,
            )
        }
        .context("vkCreatePipelineLayout")?;

        // One module per stage — naga emits entry point "main".
        let load = |spv: &[u8]| -> Result<vk::ShaderModule> {
            let words: Vec<u32> = spv
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            unsafe {
                device
                    .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                    .context("vkCreateShaderModule")
            }
        };
        let vs_mod = load(include_bytes!(concat!(env!("OUT_DIR"), "/quad_vs.spv")))?;
        let fs_mod = load(include_bytes!(concat!(env!("OUT_DIR"), "/quad_fs.spv")))?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs_mod)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fs_mod)
                .name(c"main"),
        ];
        let vi = vk::PipelineVertexInputStateCreateInfo::default();
        let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let vp = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rs = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Premultiplied src-over — the wayland shm alpha model.
        let blend_attach = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attach);
        let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
        let pci = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vi)
            .input_assembly_state(&ia)
            .viewport_state(&vp)
            .rasterization_state(&rs)
            .multisample_state(&ms)
            .color_blend_state(&cb)
            .dynamic_state(&ds)
            .layout(layout)
            .render_pass(rp)
            .subpass(0)];
        let pipeline = unsafe {
            device
                .create_graphics_pipelines(vk::PipelineCache::null(), &pci, None)
                .map_err(|(_, e)| e)
        }
        .context("vkCreateGraphicsPipelines")?[0];
        unsafe {
            device.destroy_shader_module(vs_mod, None);
            device.destroy_shader_module(fs_mod, None);
        }

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR),
                None,
            )
        }
        .context("vkCreateSampler")?;
        Ok(Self {
            render_pass: rp,
            ds_layout,
            ds_pool,
            layout,
            pipeline,
            sampler,
        })
    }
}
