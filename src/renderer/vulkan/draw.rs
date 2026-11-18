// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-surface draw queueing — texture upload through the
//! wl_buffer cache plus clip-space math for one quad.

use super::context;
use super::target::Tex;
use super::{import, upload, upload_dma, ImpTex, OutputSpec};
use crate::compositor::shm::DmaRef;
use anyhow::Result;
use ash::vk;
use std::os::fd::AsRawFd;

/// A queued draw — descriptor set + clip rect + uv rect. The
/// texture lives in the backend's cache keyed by wl_buffer id,
/// so pointer-only frames never touch the GPU allocator.
pub(super) struct Draw {
    pub(super) ds: vk::DescriptorSet,
    pub(super) ndc: [f32; 4],
    /// Texture-space crop — [0,0,1,1] draws the whole texture;
    /// wp_viewporter src rects land here.
    pub(super) uv: [f32; 4],
    /// wp_alpha_modifier multiplier — pushed to the fragment
    /// stage at offset 32 of the push-constant block.
    pub(super) alpha: f32,
}

/// Queue one texture-backed quad: upload into the wl_buffer
/// cache on first sight, allocate a descriptor set, compute
/// clip-space coords from global scene position. `dw`,`dh` is
/// the quad's on-screen extent — the viewport's dst size can
/// differ from the buffer dims. A free function taking field
/// refs — `render` holds `&self.ctx.device` for its whole body,
/// so `&mut self` methods can't borrow.
#[allow(clippy::too_many_arguments)]
pub(super) fn queue_draw(
    ctx: &context::VkContext,
    pipe: &super::pipeline::Pipe,
    cmd: vk::CommandBuffer,
    ds_pool: vk::DescriptorPool,
    arena: &mut Option<super::target::Arena>,
    tex_cache: &mut std::collections::HashMap<wayland_server::backend::ObjectId, (u64, Tex)>,
    tex_pool: &mut std::collections::HashMap<(u32, u32), Vec<Tex>>,
    draws: &mut Vec<Draw>,
    live: &mut std::collections::HashSet<wayland_server::backend::ObjectId>,
    spec: &OutputSpec,
    buf_id: wayland_server::backend::ObjectId,
    // The surface's committed content version — a mismatch
    // means the client rewrote this buffer id and the cached
    // texture holds stale pixels.
    gen: u64,
    src: *const u8,
    stride: usize,
    sw: i32,
    sh: i32,
    px: i32,
    py: i32,
    dw: i32,
    dh: i32,
    uv: [f32; 4],
    alpha: f32,
    // Buffer-space damage of this commit — `Some` lets a
    // dimension-matching cached texture patch only the changed
    // rows instead of a full re-upload. `None` = full upload.
    damage: Option<&[(i32, i32, i32, i32)]>,
    // DMA staging — `Some` uploads through
    // `vkCmdCopyBufferToImage` into OPTIMAL/VRAM textures,
    // `None` keeps the memcpy-into-linear fallback.
    mut staging: Option<&mut upload_dma::Staging>,
    // dmabuf descriptor + the import cache when this wl_buffer
    // wraps GPU memory — `Some` tries the zero-copy image import
    // before falling back to the mmap'd upload path.
    dma: Option<&DmaRef>,
    imp: &mut std::collections::HashMap<wayland_server::backend::ObjectId, ImpTex>,
    imp_bad: &mut std::collections::HashSet<wayland_server::backend::ObjectId>,
    // Replaced textures land here — destroyed/re-pooled only at
    // the retirement sweep so an in-flight submission's descriptor
    // set never points at freed memory.
    tex_retire: &mut Vec<Tex>,
    imp_retire: &mut Vec<Tex>,
    // The drawing surface's id — the shm texture cache keys on
    // it, NOT the wl_buffer id: the texture mirrors the last
    // shown frame, so commit damage (a frame diff by spec) can
    // always patch it no matter which buffer was attached.
    // A/B buffer alternation then costs a rect patch instead
    // of a full re-upload every frame.
    surf_key: wayland_server::backend::ObjectId,
) -> Result<()> {
    {
        live.insert(surf_key.clone());
        live.insert(buf_id.clone());
        // dmabuf path: the texture IS the client's memory —
        // imported once per (id, fd), every later commit just
        // re-samples. No upload ever runs for these buffers.
        if let Some(d) = dma {
            let need_new = imp
                .get(&buf_id)
                .map(|it| {
                    it.fd != d.fd.as_raw_fd() || it.tex.w != sw as u32 || it.tex.h != sh as u32
                })
                .unwrap_or(true);
            if need_new && !imp_bad.contains(&buf_id) {
                match import::import_dma_tex(ctx, cmd, d, sw.max(0) as u32, sh.max(0) as u32) {
                    Ok(tex) => {
                        let old = imp.insert(
                            buf_id.clone(),
                            ImpTex {
                                tex,
                                fd: d.fd.as_raw_fd(),
                            },
                        );
                        if let Some(old) = old {
                            // Defer — an in-flight submission may
                            // still sample the replaced import.
                            imp_retire.push(old.tex);
                        }
                    }
                    Err(e) => {
                        // A failed import leaves tiled buffers
                        // undrawable (no CPU map) — the surface
                        // vanishes. Log it once per modifier so a
                        // broken import path is visible, not silent.
                        tracing::warn!(
                            modifier = d.modifier,
                            w = sw,
                            h = sh,
                            err = %e,
                            "dmabuf import failed — surface may be blank"
                        );
                        imp_bad.insert(buf_id.clone());
                    }
                }
            }
            if let Some(it) = imp.get(&buf_id) {
                let ds = upload::alloc_ds(&ctx.device, pipe, ds_pool, it.tex.view)?;
                let (w, h) = (spec.w as f32, spec.h as f32);
                let x0 = (px - spec.ox) as f32 / w * 2.0 - 1.0;
                let y0 = 1.0 - (py - spec.oy) as f32 / h * 2.0;
                let x1 = (px + dw - spec.ox) as f32 / w * 2.0 - 1.0;
                let y1 = 1.0 - (py + dh - spec.oy) as f32 / h * 2.0;
                draws.push(Draw {
                    ds,
                    ndc: [x0, y0, x1, y1],
                    uv,
                    alpha,
                });
                return Ok(());
            }
            // Import refused → drop into the mmap'd path below.
        }
        // No CPU view and no imported texture — nothing to draw.
        if src.is_null() {
            return Ok(());
        }
        let stale = tex_cache
            .get(&surf_key)
            .map(|(g, _)| *g != gen)
            .unwrap_or(true);
        if stale {
            // New frame with client-reported damage: patch the
            // persistent texture in place — it holds exactly the
            // frame the rects diff against. Anything else takes
            // the full-upload path (size change, damage-less
            // attach, viewport, or an untracked state).
            let can_patch = damage.is_some()
                && tex_cache
                    .get(&surf_key)
                    .is_some_and(|(_, t)| t.w == sw as u32 && t.h == sh as u32);
            if can_patch {
                let (g, tex) = tex_cache.get_mut(&surf_key).unwrap();
                // A device-local texture has no host pointer —
                // it patches via staging+copy; a linear one via
                // direct row memcpy.
                if tex.ptr.is_null() {
                    if let Some(st) = staging.as_deref_mut() {
                        upload_dma::upload_rects_dma(
                            ctx,
                            cmd,
                            st,
                            tex,
                            src,
                            stride,
                            damage.unwrap(),
                        )?;
                    }
                } else {
                    upload::upload_rects(ctx, cmd, tex, src, stride, damage.unwrap());
                }
                *g = gen;
            } else {
                let fresh = match staging.as_deref_mut() {
                    Some(st) => {
                        upload_dma::upload_tex_dma(ctx, cmd, st, src, stride, sw, sh, tex_pool)?
                    }
                    None => {
                        upload::upload_tex(ctx, cmd, src, stride, sw, sh, tex_pool, arena.as_mut())?
                    }
                };
                // The replaced texture may still be sampled by an
                // in-flight submission — defer pooling/destroy to
                // the retirement sweep instead of recycling live
                // GPU memory.
                if let Some((_, old)) = tex_cache.insert(surf_key.clone(), (gen, fresh)) {
                    tex_retire.push(old);
                }
            }
        }
        let tex = &tex_cache[&surf_key].1;
        let ds = upload::alloc_ds(&ctx.device, pipe, ds_pool, tex.view)?;
        // Global coords → output-local → clip space.
        let (w, h) = (spec.w as f32, spec.h as f32);
        let x0 = (px - spec.ox) as f32 / w * 2.0 - 1.0;
        // NDC y runs bottom→top on this driver stack —
        // verified by the half-screen selftest. Window top
        // maps to +1.
        let y0 = 1.0 - (py - spec.oy) as f32 / h * 2.0;
        let x1 = (px + dw - spec.ox) as f32 / w * 2.0 - 1.0;
        let y1 = 1.0 - (py + dh - spec.oy) as f32 / h * 2.0;
        draws.push(Draw {
            ds,
            ndc: [x0, y0, x1, y1],
            uv,
            alpha,
        });
        Ok(())
    }
}

/// Queue one compositor-owned solid rect — the SSD frame parts
/// and future chrome draw as a 1×1 texture scaled to the rect,
/// so the existing quad pipeline carries them unchanged. One
/// texture per distinct color, created on first sight and kept
/// for the backend's life; descriptor sets allocate per draw
/// like every other path.
#[allow(clippy::too_many_arguments)]
pub(super) fn queue_fill(
    ctx: &context::VkContext,
    pipe: &super::pipeline::Pipe,
    ds_pool: vk::DescriptorPool,
    fills: &mut std::collections::HashMap<u32, Tex>,
    spec: &OutputSpec,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rgba: [u8; 4],
    draws: &mut Vec<Draw>,
) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Ok(());
    }
    let key = u32::from_le_bytes(rgba);
    if let std::collections::hash_map::Entry::Vacant(e) = fills.entry(key) {
        e.insert(upload::upload_once(ctx, 1, 1, &rgba)?);
    }
    let tex = &fills[&key];
    let ds = upload::alloc_ds(&ctx.device, pipe, ds_pool, tex.view)?;
    // Global coords → output-local → clip space — the same
    // transform queue_draw applies to client quads.
    let (fw, fh) = (spec.w as f32, spec.h as f32);
    let x0 = (x - spec.ox) as f32 / fw * 2.0 - 1.0;
    let y0 = 1.0 - (y - spec.oy) as f32 / fh * 2.0;
    let x1 = (x + w - spec.ox) as f32 / fw * 2.0 - 1.0;
    let y1 = 1.0 - (y + h - spec.oy) as f32 / fh * 2.0;
    draws.push(Draw {
        ds,
        ndc: [x0, y0, x1, y1],
        uv: [0.0, 0.0, 1.0, 1.0],
        alpha: 1.0,
    });
    Ok(())
}
