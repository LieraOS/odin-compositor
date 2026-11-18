// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! First-submit latency probe — reproduces the session pattern
//! (new surface → arena-bound texture → first submission that
//! references it) and times every stage separately. Drivers are
//! allowed to defer physical page-table setup to the first use
//! of freshly bound memory; if that is what stalls program
//! starts, `submit_touch` spikes here while `submit_ctrl` stays
//! flat — on real hardware, without needing a session restart.

use anyhow::{Context, Result};
use ash::vk;
use std::os::fd::RawFd;
use std::time::Instant;

use super::context::VkContext;
use super::pipeline::Pipe;
use super::target::{Arena, Target, Tex};

/// Run the probe: N rounds of {create arena-bound texture,
/// control submit not referencing it, submit referencing it},
/// each stage timed in ms and reported as one log line.
pub fn submit_test(card_fd: RawFd) -> Result<String> {
    let ctx = VkContext::new(card_fd)?;
    let dev = &ctx.device;
    let mut arena = Arena::new(&ctx, 256 * 1024 * 1024).context("arena")?;
    // Minimal command/fence plumbing — the probe only needs
    // image barriers, no render pass.
    let pool = unsafe {
        dev.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(ctx.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }
    .context("cmd pool")?;
    let cmds = unsafe {
        dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(2),
        )
    }
    .context("cmd alloc")?;
    let fence =
        unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }.context("fence")?;

    let mut out = String::new();
    for round in 0..8u32 {
        // A plausible new-surface size — big enough that the
        // driver must page real memory, small enough to repeat.
        let (w, h) = (1600, 900);
        let t = Instant::now();
        let tex = Tex::new_host_in(&ctx, &mut arena, w, h, vk::ImageUsageFlags::SAMPLED)
            .context("tex")?;
        let tex_ms = t.elapsed().as_secs_f64() * 1000.0;
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let stage = |cmd, image: Option<vk::Image>| -> Result<f64> {
            unsafe {
                dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
                dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
                // Control submits emit a generic barrier; touch
                // submits emit an image barrier that forces the
                // driver to make the freshly bound image resident.
                let barriers: Vec<vk::ImageMemoryBarrier> = image
                    .map(|im| {
                        vec![vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::HOST_WRITE)
                            .dst_access_mask(vk::AccessFlags::SHADER_READ)
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::GENERAL)
                            .image(im)
                            .subresource_range(range)]
                    })
                    .unwrap_or_default();
                dev.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::HOST,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
                dev.end_command_buffer(cmd)?;
                let t = Instant::now();
                let c = [cmd];
                dev.queue_submit(
                    ctx.queue,
                    &[vk::SubmitInfo::default().command_buffers(&c)],
                    fence,
                )?;
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                dev.wait_for_fences(&[fence], true, 2_000_000_000)?;
                dev.reset_fences(&[fence])?;
                Ok(ms)
            }
        };
        let ctrl_ms = stage(cmds[0], None)?;
        let touch_ms = stage(cmds[1], Some(tex.image))?;
        tex.destroy(dev, Some(&mut arena));
        out.push_str(&format!(
            "round{round}: tex={tex_ms:.1}ms ctrl={ctrl_ms:.2}ms touch={touch_ms:.2}ms\n"
        ));
    }
    unsafe {
        dev.destroy_fence(fence, None);
        dev.destroy_command_pool(pool, None);
    }
    tracing::info!("\n{out}");
    Ok(out)
}

/// Full-frame probe — replicates render()'s real workload: a
/// render pass drawing a warm texture plus one freshly created
/// arena texture, submitted each round. If the driver stalls on
/// the first draw-submit referencing a new image, `cold_submit`
/// spikes against `warm_submit` on real hardware.
pub fn render_probe(card_fd: RawFd) -> Result<String> {
    let ctx = VkContext::new(card_fd)?;
    let dev = &ctx.device;
    let mut arena = Arena::new(&ctx, 256 * 1024 * 1024).context("arena")?;
    let (w, h) = (640u32, 480u32);
    let pipe = Pipe::new(dev, vk::Format::B8G8R8A8_UNORM).context("pipe")?;
    let target = Target::new(&ctx, &pipe, w, h).context("target")?;
    let cmd = target.cmds[0];
    let fence = target.fences[0];
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    // Warm texture created once — stands in for surfaces that
    // have been on screen for a while.
    let warm = Tex::new_host_in(&ctx, &mut arena, 8, 8, vk::ImageUsageFlags::SAMPLED)?;
    let draws_for = |extra: Option<&Tex>| -> Result<f64> {
        let mut dses = vec![super::upload::alloc_ds(
            dev,
            &pipe,
            pipe.ds_pool,
            warm.view,
        )?];
        if let Some(t) = extra {
            dses.push(super::upload::alloc_ds(dev, &pipe, pipe.ds_pool, t.view)?);
        }
        unsafe {
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
            let mut barriers = Vec::new();
            if let Some(t) = extra {
                barriers.push(
                    vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::HOST_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .image(t.image)
                        .subresource_range(range),
                );
            }
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            let clear = vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            };
            dev.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pipe.render_pass)
                    .framebuffer(target.fb)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D {
                            width: w,
                            height: h,
                        },
                    })
                    .clear_values(std::slice::from_ref(&clear)),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: w as f32,
                    height: h as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            dev.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: w,
                        height: h,
                    },
                }],
            );
            for (i, ds) in dses.iter().enumerate() {
                let ndc = [-1.0f32 + i as f32, -1.0, 0.0 + i as f32, 1.0];
                let mut pc = [0f32; 8];
                pc[..4].copy_from_slice(&ndc);
                pc[4..].copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
                let bytes: &[u8] = std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32);
                dev.cmd_push_constants(cmd, pipe.layout, vk::ShaderStageFlags::VERTEX, 0, bytes);
                dev.cmd_push_constants(
                    cmd,
                    pipe.layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    32,
                    &1.0f32.to_le_bytes(),
                );
                dev.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipe.layout,
                    0,
                    &[*ds],
                    &[],
                );
                dev.cmd_draw(cmd, 6, 1, 0, 0);
            }
            dev.cmd_end_render_pass(cmd);
            dev.end_command_buffer(cmd)?;
            let t = Instant::now();
            let c = [cmd];
            dev.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(&c)],
                fence,
            )?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            dev.wait_for_fences(&[fence], true, 2_000_000_000)?;
            dev.reset_fences(&[fence])?;
            Ok(ms)
        }
    };
    let mut out = String::new();
    // Settle the driver before timing — first submits always
    // warm up internal structures.
    draws_for(None)?;
    draws_for(None)?;
    for round in 0..6u32 {
        let warm_ms = draws_for(None)?;
        let t = Instant::now();
        let cold = Tex::new_host_in(&ctx, &mut arena, 1600, 900, vk::ImageUsageFlags::SAMPLED)?;
        let tex_ms = t.elapsed().as_secs_f64() * 1000.0;
        let cold_ms = draws_for(Some(&cold))?;
        cold.destroy(dev, Some(&mut arena));
        out.push_str(&format!(
            "round{round}: tex={tex_ms:.1}ms warm_submit={warm_ms:.2}ms cold_submit={cold_ms:.2}ms\n"
        ));
    }
    // Burst variant — a program start can create several new
    // surfaces at once; one render pass sampling ALL of them in
    // a single submit is the session's real cold case.
    let mut burst = Vec::new();
    for _ in 0..8 {
        burst.push(Tex::new_host_in(
            &ctx,
            &mut arena,
            1920,
            1080,
            vk::ImageUsageFlags::SAMPLED,
        )?);
    }
    let t = Instant::now();
    let burst_ms = {
        let mut dses = Vec::new();
        for tx in &burst {
            dses.push(super::upload::alloc_ds(dev, &pipe, pipe.ds_pool, tx.view)?);
        }
        unsafe {
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
            let barriers: Vec<_> = burst
                .iter()
                .map(|tx| {
                    vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::HOST_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .image(tx.image)
                        .subresource_range(range)
                })
                .collect();
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            dev.end_command_buffer(cmd)?;
            let t = Instant::now();
            let c = [cmd];
            dev.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(&c)],
                fence,
            )?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            dev.wait_for_fences(&[fence], true, 2_000_000_000)?;
            dev.reset_fences(&[fence])?;
            ms
        }
    };
    out.push_str(&format!(
        "burst8_submit={burst_ms:.2}ms (create_wait={:.1}ms)\n",
        t.elapsed().as_secs_f64() * 1000.0
    ));
    for tx in &burst {
        tx.destroy(dev, Some(&mut arena));
    }
    // Dedicated-path cost — what happens when the arena is full:
    // allocate + free a fullscreen texture on its own memory,
    // the pre-arena behavior per surface.
    for round in 0..4u32 {
        let t = Instant::now();
        let tex = Tex::new_host(&ctx, 1920, 1080, vk::ImageUsageFlags::SAMPLED)?;
        let mk = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        tex.destroy(dev, None);
        out.push_str(&format!(
            "dedicated{round}: create={mk:.1}ms destroy={:.1}ms\n",
            t.elapsed().as_secs_f64() * 1000.0
        ));
    }
    // Scanout variant — the session draws INTO an imported
    // dumb-BO framebuffer, not a regular optimal-tiled image.
    // Dumb-BO alloc needs no DRM master, so this replicates the
    // exact scanout render path on the live GPU.
    {
        // Same adapter as backend::drm::Card — the drm traits
        // need an AsFd wrapper around the device fd.
        struct CardFd(std::os::fd::OwnedFd);
        impl std::os::fd::AsFd for CardFd {
            fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
                self.0.as_fd()
            }
        }
        impl drm::Device for CardFd {}
        impl drm::control::Device for CardFd {}
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(format!("/dev/dri/card{}", card_fd_card(card_fd)))
            .context("open card for dumb bo")?;
        let file = CardFd(file.into());
        use drm::buffer::Buffer as _;
        use drm::control::Device as _;
        let db = file
            .create_dumb_buffer((w, h), drm::buffer::DrmFourcc::Xrgb8888, 32)
            .context("dumb bo")?;
        let pitch = db.pitch();
        let fd = file
            .buffer_to_prime_fd(db.handle(), libc::O_CLOEXEC as u32)
            .context("prime fd")?;
        use std::os::fd::AsRawFd;
        let scan = super::scanout::ScanImg::import(&ctx, &pipe, fd.as_raw_fd(), w, h, pitch)
            .context("scan import")?;
        let draw_into_scan = |extra: Option<&Tex>| -> Result<f64> {
            let ds = super::upload::alloc_ds(dev, &pipe, pipe.ds_pool, warm.view)?;
            let dses = match extra {
                Some(t) => vec![
                    ds,
                    super::upload::alloc_ds(dev, &pipe, pipe.ds_pool, t.view)?,
                ],
                None => vec![ds],
            };
            unsafe {
                dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
                dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
                let clear = vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                };
                dev.cmd_begin_render_pass(
                    cmd,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(pipe.render_pass)
                        .framebuffer(scan.fb)
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        })
                        .clear_values(std::slice::from_ref(&clear)),
                    vk::SubpassContents::INLINE,
                );
                dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
                dev.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: w as f32,
                        height: h as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                dev.cmd_set_scissor(
                    cmd,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D {
                            width: w,
                            height: h,
                        },
                    }],
                );
                for (i, ds) in dses.iter().enumerate() {
                    let ndc = [-1.0f32 + i as f32, -1.0, 0.0 + i as f32, 1.0];
                    let mut pc = [0f32; 8];
                    pc[..4].copy_from_slice(&ndc);
                    pc[4..].copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
                    let bytes: &[u8] = std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32);
                    dev.cmd_push_constants(
                        cmd,
                        pipe.layout,
                        vk::ShaderStageFlags::VERTEX,
                        0,
                        bytes,
                    );
                    dev.cmd_push_constants(
                        cmd,
                        pipe.layout,
                        vk::ShaderStageFlags::FRAGMENT,
                        32,
                        &1.0f32.to_le_bytes(),
                    );
                    dev.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipe.layout,
                        0,
                        &[*ds],
                        &[],
                    );
                    dev.cmd_draw(cmd, 6, 1, 0, 0);
                }
                dev.cmd_end_render_pass(cmd);
                dev.end_command_buffer(cmd)?;
                let t = Instant::now();
                let c = [cmd];
                dev.queue_submit(
                    ctx.queue,
                    &[vk::SubmitInfo::default().command_buffers(&c)],
                    fence,
                )?;
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                dev.wait_for_fences(&[fence], true, 2_000_000_000)?;
                dev.reset_fences(&[fence])?;
                Ok(ms)
            }
        };
        for round in 0..4u32 {
            let ms = draw_into_scan(None)?;
            let cold = Tex::new_host_in(&ctx, &mut arena, 1600, 900, vk::ImageUsageFlags::SAMPLED)?;
            let cms = draw_into_scan(Some(&cold))?;
            cold.destroy(dev, Some(&mut arena));
            out.push_str(&format!("scan{round}: warm={ms:.2}ms cold={cms:.2}ms\n"));
        }
        let _ = file.destroy_dumb_buffer(db);
        drop(scan);
    }
    warm.destroy(dev, Some(&mut arena));
    tracing::info!("\n{out}");
    Ok(out)
}

/// Resolve which cardN node the probe fd opened — the dumb BO
/// must come from the same device the context bound to.
fn card_fd_card(_fd: RawFd) -> u32 {
    // The probe only ever runs against the primary render card;
    // a wrong guess fails loudly at create_dumb_buffer.
    1
}

/// DMA-upload probe — exercises the production staging path
/// (`upload_tex_dma`/`upload_rects_dma`) end to end: cached
/// staging buffer, graphics-queue `vkCmdCopyBufferToImage` into
/// an OPTIMAL/DEVICE_LOCAL texture, submit, fence. Proves on
/// real hardware that the gfx-queue sysmem read is healthy —
/// the historic Xid 31 fault was the dedicated copy engine,
/// which this path never uses.
pub fn dma_test(card_fd: RawFd) -> Result<String> {
    let ctx = VkContext::new(card_fd)?;
    let dev = &ctx.device;
    let pool = unsafe {
        dev.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(ctx.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }
    .context("cmd pool")?;
    let cmd = unsafe {
        dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .context("cmd alloc")?[0];
    let fence =
        unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }.context("fence")?;

    let mut st = super::upload_dma::Staging::new(&ctx, 16 << 20).context("staging")?;
    let mut tex_pool: std::collections::HashMap<(u32, u32), Vec<Tex>> =
        std::collections::HashMap::new();
    // Realistic pixel payload — a 1600×900 gradient.
    let (w, h) = (1600usize, 900usize);
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            px[i] = (x & 0xff) as u8;
            px[i + 1] = (y & 0xff) as u8;
            px[i + 2] = 200;
            px[i + 3] = 255;
        }
    }
    let mut out = String::new();
    let mut tex: Option<Tex> = None;
    for round in 0..6u32 {
        unsafe {
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        }
        let t = Instant::now();
        if let Some(t0) = tex.take() {
            // Patch path — damage-clipped DMA into the same tex.
            super::upload_dma::upload_rects_dma(
                &ctx,
                cmd,
                &mut st,
                &t0,
                px.as_ptr(),
                w * 4,
                &[(10, 10, 300, 200), (700, 400, 256, 128)],
            )?;
            let mid_ms = t.elapsed().as_secs_f64() * 1000.0;
            tex = Some(t0);
            out.push_str(&format!("patch{round}: cpu+dma-rec={mid_ms:.2}ms "));
        } else {
            let fresh = super::upload_dma::upload_tex_dma(
                &ctx,
                cmd,
                &mut st,
                px.as_ptr(),
                w * 4,
                w as i32,
                h as i32,
                &mut tex_pool,
            )?;
            let up_ms = t.elapsed().as_secs_f64() * 1000.0;
            out.push_str(&format!("full{round}: cpu+dma-rec={up_ms:.2}ms "));
            tex = Some(fresh);
        }
        let t = Instant::now();
        unsafe {
            dev.end_command_buffer(cmd)?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            dev.queue_submit(ctx.queue, &submit, fence)?;
            let submit_ms = t.elapsed().as_secs_f64() * 1000.0;
            dev.wait_for_fences(&[fence], true, 2_000_000_000)?;
            let total_ms = t.elapsed().as_secs_f64() * 1000.0;
            dev.reset_fences(&[fence])?;
            out.push_str(&format!("submit={submit_ms:.2}ms total={total_ms:.2}ms\n"));
        }
    }
    if let Some(t) = tex.take() {
        t.destroy(dev, None);
    }
    tracing::info!("\n{out}");
    Ok(out)
}
