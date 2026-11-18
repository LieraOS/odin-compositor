// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end render test for `--vk-render-test` — exercises the
//! full pipeline on real hardware without a compositor session:
//! host-linear texture upload, descriptor write, textured-quad
//! draw, compute drain readback, and a pixel assertion. This is
//! the gate that must pass before a new Vulkan build is presented
//! for session testing.

use super::{
    context::VkContext,
    drain::Drain,
    pipeline::Pipe,
    target::{Target, Tex},
    upload,
};
use anyhow::{bail, Context, Result};
use ash::vk;
use std::os::fd::RawFd;

/// Render a 2x2 solid-red texture fullscreen into a 64x64 target,
/// then verify the center pixel reads back red. Returns a summary
/// string for the probe log.
pub fn render_test(card_fd: RawFd) -> Result<String> {
    let ctx = VkContext::new(card_fd)?;
    let dev = &ctx.device;
    let (w, h) = (64u32, 64u32);
    let pipe = Pipe::new(dev, vk::Format::B8G8R8A8_UNORM).context("pipe")?;
    let target = Target::new(&ctx, &pipe, w, h).context("target")?;
    let drain = Drain::new(&ctx).context("drain")?;
    let drain_ds = drain
        .alloc_ds(
            dev,
            pipe.sampler,
            target.color.view,
            target.readbacks[0].buf,
            (w * h * 4) as u64,
        )
        .context("drain ds")?;

    // 2x2 BGRA pure red, memcpy'd into a host-linear sampled
    // image — the same upload path the session uses.
    let tex = Tex::new_host(&ctx, 2, 2, vk::ImageUsageFlags::SAMPLED).context("tex")?;
    let red_row = [0u8, 0, 255, 255, 0, 0, 255, 255];
    unsafe {
        for y in 0..2usize {
            std::ptr::copy_nonoverlapping(red_row.as_ptr(), tex.ptr.add(y * tex.pitch), 8);
        }
    }
    let ds = upload::alloc_ds(dev, &pipe, pipe.ds_pool, tex.view)?;

    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    unsafe {
        dev.reset_command_buffer(target.cmds[0], vk::CommandBufferResetFlags::empty())?;
        dev.begin_command_buffer(target.cmds[0], &vk::CommandBufferBeginInfo::default())?;
        // Host writes → shader-visible + initial layout.
        dev.cmd_pipeline_barrier(
            target.cmds[0],
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
                .subresource_range(range)],
        );
        // Clear violet, draw the red quad fullscreen — readback
        // must come back red.
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.5, 0.0, 0.5, 1.0],
            },
        };
        dev.cmd_begin_render_pass(
            target.cmds[0],
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
        dev.cmd_bind_pipeline(
            target.cmds[0],
            vk::PipelineBindPoint::GRAPHICS,
            pipe.pipeline,
        );
        dev.cmd_set_viewport(
            target.cmds[0],
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
            target.cmds[0],
            0,
            &[vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: w,
                    height: h,
                },
            }],
        );
        // Top half only — a Y-flip would land it at the bottom.
        let pc: [f32; 8] = [-1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0];
        dev.cmd_push_constants(
            target.cmds[0],
            pipe.layout,
            vk::ShaderStageFlags::VERTEX,
            0,
            std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32),
        );
        // Identity alpha — the fragment stage owns offset 32.
        dev.cmd_push_constants(
            target.cmds[0],
            pipe.layout,
            vk::ShaderStageFlags::FRAGMENT,
            32,
            &1.0f32.to_le_bytes(),
        );
        dev.cmd_bind_descriptor_sets(
            target.cmds[0],
            vk::PipelineBindPoint::GRAPHICS,
            pipe.layout,
            0,
            &[ds],
            &[],
        );
        dev.cmd_draw(target.cmds[0], 6, 1, 0, 0);
        dev.cmd_end_render_pass(target.cmds[0]);
        drain.record(
            dev,
            target.cmds[0],
            drain_ds,
            target.color.image,
            target.readbacks[0].buf,
            w,
            h,
            None,
        );
        dev.end_command_buffer(target.cmds[0])?;

        let cmds = [target.cmds[0]];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        // Target fences start SIGNALED so render() can pace on
        // them — the spec requires unsignaled at submit.
        dev.reset_fences(&[target.fences[0]])?;
        dev.queue_submit(ctx.queue, &submit, target.fences[0])
            .context("submit")?;
        dev.wait_for_fences(&[target.fences[0]], true, 5_000_000_000)
            .context("fence wait")?;
    }

    // BGRA readback — top-center must be the sampled red,
    // bottom-center the violet clear (orientation check).
    let px_at = |y: u32| -> [u8; 4] {
        let off = ((y * w + w / 2) * 4) as usize;
        unsafe {
            std::slice::from_raw_parts(target.readbacks[0].ptr.add(off), 4)
                .try_into()
                .unwrap()
        }
    };
    let top = px_at(h / 4);
    let bot = px_at(3 * h / 4);

    // --- Phase 2: clipped drain ---
    // Fill the second parity with a sentinel, replay the same
    // draw but drain only a mid-cell rect — the covered region
    // must carry live pixels, everything else must stay sentinel.
    let drain_ds2 = drain
        .alloc_ds(
            dev,
            pipe.sampler,
            target.color.view,
            target.readbacks[1].buf,
            (w * h * 4) as u64,
        )
        .context("drain ds2")?;
    unsafe {
        std::ptr::write_bytes(target.readbacks[1].ptr, 0xAA, (w * h * 4) as usize);
        dev.wait_for_fences(&[target.fences[0]], true, 5_000_000_000)?;
        dev.reset_fences(&[target.fences[0]])?;
        dev.reset_command_buffer(target.cmds[0], vk::CommandBufferResetFlags::empty())?;
        dev.begin_command_buffer(target.cmds[0], &vk::CommandBufferBeginInfo::default())?;
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.5, 0.0, 0.5, 1.0],
            },
        };
        dev.cmd_begin_render_pass(
            target.cmds[0],
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
        dev.cmd_bind_pipeline(
            target.cmds[0],
            vk::PipelineBindPoint::GRAPHICS,
            pipe.pipeline,
        );
        dev.cmd_set_viewport(
            target.cmds[0],
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
            target.cmds[0],
            0,
            &[vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: w,
                    height: h,
                },
            }],
        );
        let mut pc = [0f32; 8];
        pc[..4].copy_from_slice(&[-1.0, 1.0, 1.0, -1.0]);
        pc[4..].copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
        let pc_bytes: &[u8] = std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32);
        dev.cmd_push_constants(
            target.cmds[0],
            pipe.layout,
            vk::ShaderStageFlags::VERTEX,
            0,
            pc_bytes,
        );
        dev.cmd_push_constants(
            target.cmds[0],
            pipe.layout,
            vk::ShaderStageFlags::FRAGMENT,
            32,
            &1.0f32.to_le_bytes(),
        );
        dev.cmd_bind_descriptor_sets(
            target.cmds[0],
            vk::PipelineBindPoint::GRAPHICS,
            pipe.layout,
            0,
            &[ds],
            &[],
        );
        dev.cmd_draw(target.cmds[0], 6, 1, 0, 0);
        dev.cmd_end_render_pass(target.cmds[0]);
        // Clip a 10px rect at (20,20) — mid-cell, so the dispatch
        // must align out to the 16px grid (16..32 × 16..32).
        drain.record(
            dev,
            target.cmds[0],
            drain_ds2,
            target.color.image,
            target.readbacks[1].buf,
            w,
            h,
            Some(&[(20, 20, 10, 10)]),
        );
        dev.end_command_buffer(target.cmds[0])?;
        let cmds = [target.cmds[0]];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        dev.queue_submit(ctx.queue, &submit, target.fences[0])?;
        dev.wait_for_fences(&[target.fences[0]], true, 5_000_000_000)?;
    }
    let px2 = |x: u32, y: u32| -> [u8; 4] {
        let off = ((y * w + x) * 4) as usize;
        unsafe {
            std::slice::from_raw_parts(target.readbacks[1].ptr.add(off), 4)
                .try_into()
                .unwrap()
        }
    };
    let inside = px2(24, 24);
    let upper = px2(32, 20);
    let overshoot = px2(35, 35);
    let edge_out = px2(36, 36);
    let left_out = px2(19, 20);
    let corner = px2(16, 16);
    let outside = px2(0, 0);
    let far = px2(48, 48);
    let row = px2(24, 40);
    // Fine map — every pixel, downsampled to 8px cells.
    let mut map = String::new();
    for gy in (0..h).step_by(8) {
        for gx in (0..w).step_by(8) {
            let mut hit = false;
            for y in gy..(gy + 8).min(h) {
                for x in gx..(gx + 8).min(w) {
                    if px2(x, y) != [0xAA; 4] {
                        hit = true;
                    }
                }
            }
            map.push(if hit { '#' } else { '.' });
        }
        map.push('\n');
    }
    // What the host actually wrote into the params slots.
    let prm: Vec<u32> =
        unsafe { std::slice::from_raw_parts(drain.params.ptr as *const u32, 32).to_vec() };
    tracing::info!("\nclip map:\n{map}\nparams[0..32]={prm:?}");
    tex.destroy(dev, None);
    drain.destroy(dev);
    target.destroy(dev);
    unsafe { dev.device_wait_idle() }?;
    let (_b, _g, r) = (top[0], top[1], top[2]);
    if r < 200 || top[0] > 60 {
        bail!(
            "render test failed — top pixel bgra=({},{},{}) expected red",
            top[0],
            top[1],
            top[2]
        );
    }
    if bot[0] < 100 || bot[2] < 100 || bot[1] > 60 {
        bail!(
            "orientation check failed — bottom pixel bgra=({},{},{}) expected violet",
            bot[0],
            bot[1],
            bot[2]
        );
    }
    // Phase 2 — clipped drain: the dispatch covers exactly the
    // clip rect (20..36 × 20..36 — one 16px workgroup at offset
    // (20,20)). Covered pixels must be live frame content (red
    // top half, violet bottom), everything else stays sentinel.
    for (name, px) in [("inside", inside), ("upper", upper)] {
        if px[2] < 200 || px[0] > 60 {
            bail!(
                "clip drain: {name} pixel bgra=({},{},{}) expected red",
                px[0],
                px[1],
                px[2]
            );
        }
    }
    if overshoot == [0xAA; 4] {
        bail!("clip drain: overshoot pixel still sentinel — dispatch undersized");
    }
    for (name, px) in [
        ("edge-out", edge_out),
        ("left-out", left_out),
        ("corner", corner),
        ("origin", outside),
        ("far", far),
        ("other-row", row),
    ] {
        if px != [0xAA; 4] {
            bail!(
                "clip drain: {name} pixel bgra=({},{},{}) expected sentinel 0xaa",
                px[0],
                px[1],
                px[2]
            );
        }
    }
    // Direct-scanout capability record — the import path needs
    // both extensions to land dmabuf memory as a render target.
    Ok(format!(
        "vulkan render test ok — top bgra=({},{},{}), drm-modifier-ext={}, dmabuf-ext={}",
        top[0], top[1], top[2], ctx.has_modifier_ext, ctx.has_dma_buf_ext
    ))
}
