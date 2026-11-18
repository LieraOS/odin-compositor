// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame drain — moves the finished frame from the device-local
//! color attachment into the host-mappable readback buffer using
//! a compute dispatch. The graphics/compute engine does the whole
//! move: on drivers where the copy engine cannot touch sysmem
//! (observed: Xid 31 MMU faults), this still works because CE is
//! never involved. On drivers with a healthy CE it is equally
//! valid — one dispatch per frame is cheap.

use anyhow::{Context, Result};
use ash::vk;

use super::context::VkContext;
use super::target::HostBuf;

/// Maximum damage rects a single clipped drain dispatches — each
/// gets its own uniform slot. More damage falls back to a
/// full-frame drain (correct, just unclipped).
const MAX_CLIP_SLOTS: usize = 32;

pub struct Drain {
    pub ds_layout: vk::DescriptorSetLayout,
    pub ds_pool: vk::DescriptorPool,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    /// Host-visible per-rect clip offsets — the shader reads the
    /// slot selected by the descriptor's dynamic offset.
    pub(crate) params: HostBuf,
    /// Byte stride between param slots — the device's
    /// `minUniformBufferOffsetAlignment`, at least 16.
    slot_stride: u64,
}

impl Drain {
    pub fn new(ctx: &VkContext) -> Result<Self> {
        let dev = &ctx.device;
        let slot_stride = ctx.ubo_align;
        let ds_layout = unsafe {
            dev.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(0)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE),
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(1)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE),
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE),
                    // Per-rect clip offset — the descriptor binds the
                    // params buffer; the dynamic offset selects the
                    // slot at dispatch time.
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(3)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE),
                ]),
                None,
            )
            .context("drain ds_layout")?
        };
        let ds_pool = unsafe {
            dev.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(8)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLED_IMAGE,
                            descriptor_count: 8,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLER,
                            descriptor_count: 8,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: 8,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC,
                            descriptor_count: 8,
                        },
                    ]),
                None,
            )
            .context("drain ds_pool")?
        };
        let layouts = [ds_layout];
        let layout = unsafe {
            dev.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
                None,
            )
        }?;
        let spv = include_bytes!(concat!(env!("OUT_DIR"), "/drain_cs.spv"));
        let words: Vec<u32> = spv
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let module = unsafe {
            dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .context("drain shader module")?
        };
        let entry = c"main";
        let pipeline = unsafe {
            dev.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(module)
                            .name(entry),
                    )
                    .layout(layout)],
                None,
            )
            .map_err(|e| e.1)
            .context("drain compute pipeline")?[0]
        };
        unsafe { dev.destroy_shader_module(module, None) };
        // One clip-offset slot per damage rect, strided by the
        // device's uniform-buffer offset alignment.
        let params = HostBuf::new(
            ctx,
            MAX_CLIP_SLOTS * slot_stride as usize,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
        )
        .context("drain params")?;
        Ok(Self {
            ds_layout,
            ds_pool,
            layout,
            pipeline,
            params,
            slot_stride,
        })
    }

    /// Descriptor set sampling `src` and writing `dst` — allocated
    /// once per target since both bindings are fixed for its life.
    pub fn alloc_ds(
        &self,
        dev: &ash::Device,
        sampler: vk::Sampler,
        src_view: vk::ImageView,
        dst_buf: vk::Buffer,
        dst_size: u64,
    ) -> Result<vk::DescriptorSet> {
        let ds = unsafe {
            dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.ds_pool)
                    .set_layouts(&[self.ds_layout]),
            )?[0]
        };
        let img = [vk::DescriptorImageInfo::default()
            .image_view(src_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let smp = [vk::DescriptorImageInfo::default().sampler(sampler)];
        let buf = [vk::DescriptorBufferInfo::default()
            .buffer(dst_buf)
            .offset(0)
            .range(dst_size)];
        // Dynamic-UBO binding — the written offset/range describe
        // ONE slot; `cmd_bind_descriptor_sets` shifts it per rect.
        let prm = [vk::DescriptorBufferInfo::default()
            .buffer(self.params.buf)
            .offset(0)
            .range(16)];
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
                    vk::WriteDescriptorSet::default()
                        .dst_set(ds)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&buf),
                    vk::WriteDescriptorSet::default()
                        .dst_set(ds)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                        .buffer_info(&prm),
                ],
                &[],
            );
        }
        Ok(ds)
    }

    /// Record the drain after the render pass: RP writes become
    /// shader-visible, the dispatch packs BGRA into the readback
    /// buffer, a final barrier makes writes host-visible.
    ///
    /// `clip` limits the readback to damage rects (output-local
    /// pixels) — `None` drains the whole frame. A clipped drain
    /// dispatches once per rect; before each dispatch the
    /// descriptor is re-bound with a dynamic offset pointing at
    /// that rect's uniform slot, whose `[x0, y0]` the shader adds
    /// to its invocation id. The host writes the slots into the
    /// persistent-mapped params buffer at record time — coherent
    /// memory plus the submit boundary make them visible when the
    /// dispatches run. Overflowing the slot table falls back to a
    /// full drain so no damage is ever skipped.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        ds: vk::DescriptorSet,
        color: vk::Image,
        dst_buf: vk::Buffer,
        w: u32,
        h: u32,
        clip: Option<&[(i32, i32, i32, i32)]>,
    ) {
        unsafe {
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(color)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })],
            );
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            // Slot writes land in persistent-mapped coherent memory
            // at record time; the submit boundary orders them before
            // the GPU reads them at dispatch.
            let slot = |i: usize, off: [u32; 2]| {
                (self.params.ptr.add(i * self.slot_stride as usize) as *mut [u32; 2])
                    .write_unaligned(off)
            };
            match clip {
                None => {
                    slot(0, [0, 0]);
                    dev.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::COMPUTE,
                        self.layout,
                        0,
                        &[ds],
                        &[0],
                    );
                    dev.cmd_dispatch(cmd, w.div_ceil(16), h.div_ceil(16), 1);
                }
                Some(rects) if rects.len() <= MAX_CLIP_SLOTS => {
                    let mut slot_i = 0usize;
                    for &(rx, ry, rw, rh) in rects {
                        let x0 = rx.max(0);
                        let y0 = ry.max(0);
                        let x1 = rx.saturating_add(rw).clamp(0, w as i32);
                        let y1 = ry.saturating_add(rh).clamp(0, h as i32);
                        if x1 <= x0 || y1 <= y0 {
                            continue;
                        }
                        slot(slot_i, [x0 as u32, y0 as u32]);
                        dev.cmd_bind_descriptor_sets(
                            cmd,
                            vk::PipelineBindPoint::COMPUTE,
                            self.layout,
                            0,
                            &[ds],
                            &[slot_i as u32 * self.slot_stride as u32],
                        );
                        dev.cmd_dispatch(
                            cmd,
                            ((x1 - x0) as u32).div_ceil(16),
                            ((y1 - y0) as u32).div_ceil(16),
                            1,
                        );
                        slot_i += 1;
                    }
                }
                // More damage rects than slots — drain everything.
                Some(_) => {
                    slot(0, [0, 0]);
                    dev.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::COMPUTE,
                        self.layout,
                        0,
                        &[ds],
                        &[0],
                    );
                    dev.cmd_dispatch(cmd, w.div_ceil(16), h.div_ceil(16), 1);
                }
            }
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)
                    .buffer(dst_buf)
                    .size(vk::WHOLE_SIZE)],
                &[],
            );
        }
    }

    pub fn destroy(&self, dev: &ash::Device) {
        unsafe {
            self.params.destroy(dev);
            dev.destroy_pipeline(self.pipeline, None);
            dev.destroy_pipeline_layout(self.layout, None);
            dev.destroy_descriptor_pool(self.ds_pool, None);
            dev.destroy_descriptor_set_layout(self.ds_layout, None);
        }
    }
}
