// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame path — upload, draw, readback, retire. Split from
//! `mod.rs` to keep the module index under the file-size rule.

use super::draw::{queue_draw, queue_fill, Draw};
use super::target::Tex;
use super::{scanout, upload, Backend, OutputSpec, VulkanBackend};
use anyhow::Result;
use ash::vk;
use wayland_server::Resource;
impl Backend for VulkanBackend {
    /// Pipelined: the returned slice is the previous frame's
    /// readback — the scene state it was submitted against is
    /// one damage set behind `present_damage`.
    fn latency(&self) -> usize {
        1
    }

    /// Fence status check — retires every in-flight parity whose
    /// fence already signaled and hands out the newest one's
    /// pixels, so idle ticks still present the latest frame
    /// without touching the GPU queue.
    fn take_retired(&mut self) -> Option<*const u8> {
        let mut newest: Option<usize> = None;
        let mut newest_seq = 0u64;
        for i in 0..super::PARITIES {
            if !self.in_flight[i] {
                continue;
            }
            if matches!(
                unsafe { self.ctx.device.get_fence_status(self.target.fences[i]) },
                Ok(true)
            ) {
                self.in_flight[i] = false;
                // This parity's pixels now hold every damage set
                // its submission covered — raise the presentable
                // bound so the queue may drain them. The bound
                // only ever grows: two parities can retire in one
                // pass, and iteration order is NOT submission
                // order once `cur` wrapped — taking the last
                // index would regress the bound and present a
                // parity older than the newest retired one.
                self.presentable = self.presentable.max(self.cover_seq[i]);
                // Staging buffers replaced mid-frame are no longer
                // referenced by any in-flight copy command.
                if let Some(st) = self.staging.get_mut(i) {
                    st.drain_retired(&self.ctx.device);
                }
                // Newest = highest cover_seq, not highest index —
                // same-queue fences signal in submission order,
                // so the max-seq retiree holds the latest pixels.
                if self.cover_seq[i] >= newest_seq {
                    newest_seq = self.cover_seq[i];
                    newest = Some(i);
                }
            }
        }
        let idx = newest?;
        self.last_out = Some(idx);
        Some(self.target.readbacks[idx].ptr as *const u8)
    }

    /// Damage-set sequence bound of the presentable parity.
    fn presentable_seq(&self) -> u64 {
        self.presentable
    }

    /// Import-side direct scanout — each plane is a DRM dumb BO;
    /// its dmabuf fd becomes a render target the draw phase writes
    /// straight into scanout memory. One failed import kills the
    /// whole path (partial parities can never flip safely).
    fn attach_scanout(&mut self, planes: &[crate::renderer::DmaBufPlane]) -> bool {
        use std::os::fd::AsRawFd;
        if planes.len() != super::PARITIES || self.scan.is_some() {
            return self.scan.is_some();
        }
        let mut imgs: Vec<scanout::ScanImg> = Vec::new();
        for p in planes {
            match scanout::ScanImg::import(
                &self.ctx,
                &self.pipe,
                p.fd.as_raw_fd(),
                p.w,
                p.h,
                p.pitch,
            ) {
                Ok(i) => imgs.push(i),
                Err(err) => {
                    tracing::warn!(?err, "scanout import failed — CPU-copy present stays");
                    for i in imgs {
                        i.destroy(&self.ctx.device);
                    }
                    return false;
                }
            }
        }
        match <[scanout::ScanImg; super::PARITIES]>::try_from(imgs) {
            Ok(arr) => {
                self.scan = Some(arr);
                tracing::info!("vulkan direct scanout: rendering into imported dumb BOs");
                true
            }
            Err(v) => {
                for i in v {
                    i.destroy(&self.ctx.device);
                }
                false
            }
        }
    }

    /// Parity of the last pixels handed out — the index the
    /// Output's scan_fbs array mirrors.
    fn last_retired(&self) -> Option<usize> {
        self.last_out
    }

    fn render(
        &mut self,
        comp: &mut crate::compositor::CompState,
        spec: OutputSpec,
        cover_clip: Option<&[(i32, i32, i32, i32)]>,
        cover_seq: u64,
        blocked: u8,
        tick: &mut dyn FnMut(&mut crate::compositor::CompState),
    ) -> Result<&[u8]> {
        use crate::compositor::SurfaceData;
        let dev = &self.ctx.device;
        let (w, h) = (spec.w, spec.h);

        // Pace: this frame records into parity `cur`, so it only
        // waits on `cur`'s previous submission — three frames old
        // and almost always already signaled. The newest in-flight
        // parity is never waited on: the GPU pipelines freely and
        // the CPU stays unblocked (same model as every real
        // compositor renderer). Poll in ~8 ms slices and yield to
        // `tick` between them.
        let t_wait = std::time::Instant::now();
        // Retire every in-flight parity whose fence signaled —
        // a nonblocking status poll; `retired_idx` tracks the
        // newest retiree: its pixels present this frame.
        let mut retired_idx = None;
        let mut retired_seq = 0u64;
        unsafe {
            for i in 0..super::PARITIES {
                if self.in_flight[i]
                    && matches!(dev.get_fence_status(self.target.fences[i]), Ok(true))
                {
                    self.in_flight[i] = false;
                    // Same ordering rule as take_retired: index
                    // order is not submission order once `cur`
                    // wrapped — the presentable bound must never
                    // regress, and the presented parity must be
                    // the newest retiree (max cover_seq), not the
                    // last index the loop happened to visit.
                    self.presentable = self.presentable.max(self.cover_seq[i]);
                    if self.cover_seq[i] >= retired_seq {
                        retired_seq = self.cover_seq[i];
                        retired_idx = Some(i);
                    }
                }
            }
        }
        // Parity is ONE coherent slot — cmd buffer, ds pool,
        // fence and scan target all belong to `cur`. Pick it
        // BEFORE touching any of them: resetting a fence and
        // then rotating to a different parity would orphan the
        // reset fence (unsignaled forever → every later wait on
        // it eats the full timeout → forced CPU demotion).
        // Candidates: not CRTC-scanned or flip-pending (blocked
        // bitmask) and not the just-retired parity whose pixels
        // present this frame.
        let mut candidates = [usize::MAX; super::PARITIES];
        let mut n = 0;
        for i in 0..super::PARITIES {
            let c = (self.cur + i) % super::PARITIES;
            if blocked & (1 << c) == 0 && Some(c) != retired_idx {
                candidates[n] = c;
                n += 1;
            }
        }
        if n == 0 {
            // No writable parity — re-present the newest retired
            // one instead of scribbling into scanout.
            let out = retired_idx.or(self.last_out).unwrap_or(self.cur);
            self.last_out = Some(out);
            return Ok(unsafe {
                std::slice::from_raw_parts(self.target.readbacks[out].ptr, (w * h * 4) as usize)
            });
        }
        // Prefer an idle slot — its previous submission is known
        // complete, so the CPU waits on nothing at all. Only when
        // every candidate is still executing does the frame wait,
        // sliced ~8 ms so input keeps flowing.
        self.cur = match candidates[..n]
            .iter()
            .copied()
            .find(|&c| !self.in_flight[c])
        {
            Some(c) => c,
            None => {
                let c = candidates[0];
                unsafe {
                    loop {
                        match dev.wait_for_fences(&[self.target.fences[c]], true, 8_000_000) {
                            Ok(()) => break,
                            Err(vk::Result::TIMEOUT) => {
                                // 1 s outer bound, same as before
                                // — only sliced so input drains.
                                if t_wait.elapsed().as_millis() >= 1_000 {
                                    return Err(vk::Result::TIMEOUT.into());
                                }
                                tick(comp);
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                }
                // The slot is free now; its old pixels die with
                // this frame's overwrite, so it is NOT a retiree.
                self.in_flight[c] = false;
                c
            }
        };
        comp.stat_wait_ms = comp
            .stat_wait_ms
            .max(t_wait.elapsed().as_secs_f64() * 1000.0);
        unsafe {
            dev.reset_fences(&[self.target.fences[self.cur]])?;
            dev.reset_command_buffer(
                self.target.cmds[self.cur],
                vk::CommandBufferResetFlags::empty(),
            )?;
            // Per-parity pool — a reset frees only descriptors a
            // retired submission could still reference; sets bound
            // into in-flight parities live in their own pools.
            dev.reset_descriptor_pool(
                self.target.ds_pools[self.cur],
                vk::DescriptorPoolResetFlags::empty(),
            )?;
            dev.begin_command_buffer(
                self.target.cmds[self.cur],
                &vk::CommandBufferBeginInfo::default(),
            )?;
        }
        let cmd = self.target.cmds[self.cur];
        let ds_pool = self.target.ds_pools[self.cur];
        let t_upload = std::time::Instant::now();

        // Staging bump cursor — this parity's fence retired, so
        // its last submission can no longer read the buffer.
        if let Some(st) = self.staging.get_mut(self.cur) {
            st.rewind();
        }

        // --- Upload phase: every drawable is memcpy'd into a
        // host-visible linear texture — no copy engine anywhere.
        // Long upload bursts yield to `tick` every ~8ms so input
        // drains and the cursor plane move inside the stall
        // instead of freezing for the whole upload.
        let mut draws: Vec<Draw> = Vec::new();
        let mut live: std::collections::HashSet<wayland_server::backend::ObjectId> =
            std::collections::HashSet::new();
        let mut t_tick = std::time::Instant::now();
        // DMA staging handle for this parity — `None` when the
        // linear fallback is active (env switch or alloc fail).
        let mut staging = self.staging.get_mut(self.cur);
        // Surfaces in stacking order — the shared scene() list:
        // layers below/above windows, popups, session-lock gating
        // and minimize/workspace visibility are already applied.
        for item in comp.scene() {
            // Compositor-owned fill slots — SSD frame quads get
            // a cached 1×1 texture per color and draw like any
            // other quad, no client surface involved.
            let (item_surf, item_x, item_y) = match item {
                crate::compositor::render::SceneItem::Surf { surf, x, y } => (surf, x, y),
                crate::compositor::render::SceneItem::Fill { x, y, w, h, rgba } => {
                    queue_fill(
                        &self.ctx,
                        &self.pipe,
                        ds_pool,
                        &mut self.fill_texs,
                        &spec,
                        x,
                        y,
                        w,
                        h,
                        rgba,
                        &mut draws,
                    )?;
                    continue;
                }
            };
            let Some(sd) = item_surf.data::<SurfaceData>() else {
                continue;
            };
            // Cursor-role surfaces are drawn by the pointer path,
            // never as scene elements.
            if sd.is_cursor.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            let (src, stride, bw, bh, buf_id, dma) = {
                let shown = sd.shown_buf.lock().unwrap();
                match shown
                    .as_ref()
                    .and_then(|b| b.data::<crate::compositor::BufInfo>())
                {
                    Some(bi) => {
                        let map = bi.map.lock().unwrap();
                        // Guard the raw-pointer upload source: a
                        // failed pool mmap maps to null and a
                        // buffer must still fit the (current) map
                        // — the upload reads (h-1)*stride + w*4
                        // bytes starting at offset. A null map is
                        // only fatal when no dmabuf fd can take
                        // over — the import path reads zero CPU
                        // bytes.
                        let end = bi.offset as i64
                            + (bi.h as i64 - 1) * bi.stride as i64
                            + bi.w as i64 * 4;
                        if bi.offset < 0
                            || end > map.len as i64
                            || (map.ptr.is_null() && bi.dma.is_none())
                        {
                            continue;
                        }
                        // A dup'd fd lets the import path outlive
                        // this borrow scope — one dup syscall per
                        // drawn surface per frame is noise.
                        let dma = bi.dma.as_ref().and_then(|d| {
                            d.fd.try_clone()
                                .ok()
                                .map(|fd| crate::compositor::shm::DmaRef {
                                    fd,
                                    offset: d.offset,
                                    stride: d.stride,
                                    modifier: d.modifier,
                                    format: d.format,
                                })
                        });
                        (
                            if map.ptr.is_null() {
                                std::ptr::null()
                            } else {
                                unsafe { map.ptr.offset(bi.offset as isize) }
                            },
                            bi.stride.max(bi.w * 4) as usize,
                            bi.w,
                            bi.h,
                            shown
                                .as_ref()
                                .map(|b| b.id())
                                .unwrap_or_else(|| item_surf.id()),
                            dma,
                        )
                    }
                    None => continue,
                }
            };
            // Upload clip — the per-surface texture mirrors the
            // last shown frame, so commit damage (a frame diff)
            // always legitimately patches it. The state is read
            // and marked synced under one lock: any later commit
            // accumulates on a clean base, and a commit racing
            // this read can't tear because its rects cover the
            // buffer we then upload in full anyway.
            let gen = sd.commit_gen.load(std::sync::atomic::Ordering::SeqCst);
            let upload_dmg = {
                let mut u = sd.upload_damage.lock().unwrap();
                let rects = if u.0 == gen { u.2.clone() } else { None };
                u.2 = Some(Vec::new());
                rects
            };
            // wp_viewporter — committed crop+scale: the src rect
            // becomes the uv window, the dst size the quad extent.
            let vp = *sd.vp.lock().unwrap();
            let (sx, sy, sw, sh) = vp.src.unwrap_or((0.0, 0.0, bw as f64, bh as f64));
            let (dw, dh) = vp.dst.unwrap_or((sw.ceil() as i32, sh.ceil() as i32));
            let uv = [
                (sx / bw as f64) as f32,
                (sy / bh as f64) as f32,
                ((sx + sw) / bw as f64) as f32,
                ((sy + sh) / bh as f64) as f32,
            ];
            // Skip surfaces this output can never paint — scene()
            // is global, so without the clip every surface would
            // upload its pixels into EVERY output's texture cache
            // (a 9 MB window costs ~18 MB of DMA per frame across
            // two monitors). Textures evict lazily, so a surface
            // sliding back onscreen just re-uploads once.
            if item_x + dw <= spec.ox
                || item_y + dh <= spec.oy
                || item_x >= spec.ox + spec.w as i32
                || item_y >= spec.oy + spec.h as i32
            {
                continue;
            }
            queue_draw(
                &self.ctx,
                &self.pipe,
                cmd,
                ds_pool,
                &mut self.arena,
                &mut self.tex_cache,
                &mut self.tex_pool,
                &mut draws,
                &mut live,
                &spec,
                buf_id,
                gen,
                src,
                stride,
                bw,
                bh,
                item_x,
                item_y,
                dw,
                dh,
                uv,
                // Committed wp_alpha_modifier — None is identity.
                sd.alpha.lock().unwrap().unwrap_or(1.0) as f32,
                upload_dmg.as_deref(),
                staging.as_deref_mut(),
                dma.as_ref(),
                &mut self.imp_cache,
                &mut self.imp_bad,
                &mut self.tex_retire,
                &mut self.imp_retire,
                item_surf.id(),
            )?;
            // Cooperative yield — keep input + cursor live
            // during heavy upload bursts.
            if t_tick.elapsed().as_millis() >= 8 {
                t_tick = std::time::Instant::now();
                tick(comp);
            }
        }

        // The pointer — themed cursor image first (same precedence
        // as the CPU path), then a client cursor surface, then the
        // fallback arrow. A hardware cursor plane owns this
        // output's pointer — skip every software path.
        let mut cursor_done = comp.hw_cursor.contains(&spec.name);
        // A hardware cursor plane owns this output — the themed
        // image must NOT be composited too, or the pointer shows
        // twice: the plane's live position plus a frame-behind
        // ghost baked into the scanout copy.
        if !cursor_done {
            if let Some(img) = comp.cursor_img.clone() {
                // Themed cursors are tiny — rewrite the persistent
                // host texture per frame, recreating only on size
                // changes, and re-tag the host writes for the shader.
                let needs_new = self
                    .cursor_tex
                    .as_ref()
                    .map(|(t, ..)| t.w != img.w || t.h != img.h)
                    .unwrap_or(true);
                if needs_new {
                    if let Some((old, ..)) = self.cursor_tex.take() {
                        old.destroy(&self.ctx.device, self.arena.as_mut());
                    }
                    self.cursor_tex = Some((
                        Tex::new_host(&self.ctx, img.w, img.h, vk::ImageUsageFlags::SAMPLED)?,
                        img.hx,
                        img.hy,
                    ));
                }
                let (tex, hx, hy) = self.cursor_tex.as_ref().unwrap();
                unsafe {
                    for y in 0..img.h as usize {
                        std::ptr::copy_nonoverlapping(
                            img.pixels.as_ptr().add(y * img.w as usize * 4),
                            tex.ptr.add(y * tex.pitch),
                            img.w as usize * 4,
                        );
                    }
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
                let ds = upload::alloc_ds(dev, &self.pipe, ds_pool, tex.view)?;
                let px = comp.ptr_pos.0 as i32 - hx;
                let py = comp.ptr_pos.1 as i32 - hy;
                draws.push(Draw {
                    ds,
                    alpha: 1.0,
                    ndc: [
                        (px - spec.ox) as f32 / w as f32 * 2.0 - 1.0,
                        1.0 - (py - spec.oy) as f32 / h as f32 * 2.0,
                        (px + img.w as i32 - spec.ox) as f32 / w as f32 * 2.0 - 1.0,
                        1.0 - (py + img.h as i32 - spec.oy) as f32 / h as f32 * 2.0,
                    ],
                    uv: [0.0, 0.0, 1.0, 1.0],
                });
                cursor_done = true;
            }
        }
        if !cursor_done {
            if let Some(cur) = comp.cursor_surf.as_ref().filter(|s| s.is_alive()).cloned() {
                if let Some(sd) = cur.data::<SurfaceData>() {
                    let shown = sd.shown_buf.lock().unwrap();
                    if let Some(bi) = shown
                        .as_ref()
                        .and_then(|b| b.data::<crate::compositor::BufInfo>())
                    {
                        let map = bi.map.lock().unwrap();
                        // Same upload-source guard as the surface
                        // loop — the cursor buffer reads
                        // (h-1)*stride + w*4 bytes from offset.
                        let end = bi.offset as i64
                            + (bi.h as i64 - 1) * bi.stride as i64
                            + bi.w as i64 * 4;
                        if !map.ptr.is_null() && bi.offset >= 0 && end <= map.len as i64 {
                            let src = unsafe { map.ptr.offset(bi.offset as isize) };
                            let buf_id = shown.as_ref().map(|b| b.id()).unwrap_or_else(|| cur.id());
                            queue_draw(
                                &self.ctx,
                                &self.pipe,
                                cmd,
                                ds_pool,
                                &mut self.arena,
                                &mut self.tex_cache,
                                &mut self.tex_pool,
                                &mut draws,
                                &mut live,
                                &spec,
                                buf_id,
                                sd.commit_gen.load(std::sync::atomic::Ordering::SeqCst),
                                src,
                                bi.stride.max(bi.w * 4) as usize,
                                bi.w,
                                bi.h,
                                comp.ptr_pos.0 as i32 - comp.cursor_hotspot.0,
                                comp.ptr_pos.1 as i32 - comp.cursor_hotspot.1,
                                bi.w,
                                bi.h,
                                [0.0, 0.0, 1.0, 1.0],
                                // Client cursor surfaces keep their own
                                // alpha modifier like any surface.
                                sd.alpha.lock().unwrap().unwrap_or(1.0) as f32,
                                // Cursor images are tiny — a full
                                // upload costs nothing.
                                None,
                                staging.as_deref_mut(),
                                // Client cursors are shm in
                                // practice — no dmabuf import.
                                None,
                                &mut self.imp_cache,
                                &mut self.imp_bad,
                                &mut self.tex_retire,
                                &mut self.imp_retire,
                                cur.id(),
                            )?;
                            cursor_done = true;
                        }
                    }
                }
            }
        }
        if !cursor_done {
            let ds = upload::alloc_ds(dev, &self.pipe, ds_pool, self.arrow.view)?;
            let px = comp.ptr_pos.0 as i32 - spec.ox;
            let py = comp.ptr_pos.1 as i32 - spec.oy;
            draws.push(Draw {
                ds,
                ndc: [
                    px as f32 / w as f32 * 2.0 - 1.0,
                    1.0 - py as f32 / h as f32 * 2.0,
                    (px + 12) as f32 / w as f32 * 2.0 - 1.0,
                    1.0 - (py + 19) as f32 / h as f32 * 2.0,
                ],
                uv: [0.0, 0.0, 1.0, 1.0],
                // The fallback arrow draws fully opaque.
                alpha: 1.0,
            });
        }

        comp.stat_upload_ms = comp
            .stat_upload_ms
            .max(t_upload.elapsed().as_secs_f64() * 1000.0);
        // Upload volume this frame — surfaces the "time is all
        // memcpy" metric can't tell apart (one big patch vs N
        // full-screen uploads look identical in ms alone).
        if let Some(st) = staging.as_deref_mut() {
            comp.stat_up_kb = comp.stat_up_kb.max(st.up_bytes / 1024);
            comp.stat_up_texs = comp.stat_up_texs.max((st.up_full + st.up_patch) as u64);
            if st.up_big.0 / 1024 > comp.stat_up_big_kb {
                comp.stat_up_big_kb = st.up_big.0 / 1024;
                comp.stat_up_big_w = st.up_big.1;
                comp.stat_up_big_h = st.up_big.2;
            }
            st.up_bytes = 0;
            st.up_full = 0;
            st.up_patch = 0;
            st.up_big = (0, 0, 0);
        }

        // --- Draw phase ---
        let t_draw = std::time::Instant::now();
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.25, 0.06, 0.125, 1.0],
            },
        };
        unsafe {
            dev.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.pipe.render_pass)
                    // Direct scanout: the imported dumb-BO image
                    // receives the draw — the whole readback+drain
                    // below is skipped and present only page-flips.
                    .framebuffer(
                        self.scan
                            .as_ref()
                            .map(|s| s[self.cur].fb)
                            .unwrap_or(self.target.fb),
                    )
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
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipe.pipeline);
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
            for d in &draws {
                let mut pc = [0f32; 8];
                pc[..4].copy_from_slice(&d.ndc);
                pc[4..].copy_from_slice(&d.uv);
                let bytes: &[u8] = std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32);
                dev.cmd_push_constants(
                    cmd,
                    self.pipe.layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    bytes,
                );
                // Fragment stage owns offset 32 — the
                // wp_alpha_modifier multiplier.
                dev.cmd_push_constants(
                    cmd,
                    self.pipe.layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    32,
                    &d.alpha.to_le_bytes(),
                );
                dev.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipe.layout,
                    0,
                    &[d.ds],
                    &[],
                );
                dev.cmd_draw(cmd, 6, 1, 0, 0);
            }
            dev.cmd_end_render_pass(cmd);
            // In scanout mode the render pass already wrote the
            // CRTC's buffer — draining it anywhere would be a pure
            // copy of pixels nobody reads.
            if self.scan.is_none() {
                self.drain.record(
                    dev,
                    cmd,
                    self.drain_ds[self.cur],
                    self.target.color.image,
                    self.target.readbacks[self.cur].buf,
                    w,
                    h,
                    cover_clip,
                );
            }
            dev.end_command_buffer(cmd)?;
            comp.stat_draw_ms = comp
                .stat_draw_ms
                .max(t_draw.elapsed().as_secs_f64() * 1000.0);
            // Submit timing is its own stat: drivers may defer
            // memory materialization to the first submit touching
            // a freshly bound image, which would show up here and
            // nowhere else.
            let t_submit = std::time::Instant::now();
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            dev.queue_submit(self.ctx.queue, &submit, self.target.fences[self.cur])?;
            comp.stat_submit_ms = comp
                .stat_submit_ms
                .max(t_submit.elapsed().as_secs_f64() * 1000.0);
            self.in_flight[self.cur] = true;
            // The clip this submission drained covers every queued
            // damage set — remember the bound so presents only
            // copy regions the readback actually contains.
            self.cover_seq[self.cur] = cover_seq;
            // Deliberately no fence wait here: the CPU is handed
            // an older, already-retired parity — blocking after
            // submit would serialize the event loop on a full
            // GPU roundtrip and starve input dispatch whenever
            // a client commits every frame (menu hover). Only
            // the very first frame waits on its own submit —
            // before it, no parity holds real pixels.
            if self.first_present {
                dev.wait_for_fences(&[self.target.fences[self.cur]], true, 1_000_000_000)?;
                self.in_flight[self.cur] = false;
                // We present our own parity — its coverage bound
                // applies to the pixels being returned.
                self.presentable = self.cover_seq[self.cur];
            }
        }
        // Recycle textures whose wl_buffer vanished — only safe
        // once no submission still executing could sample them:
        // a texture freed mid-flight is a use-after-free on the
        // GPU. `cur` is exempt from the gate: the frame it just
        // submitted can only sample `live` textures — it drew
        // them. Stale entries were last sampled by older
        // parities, so those are the ones that must be retired.
        let busy = self
            .in_flight
            .iter()
            .enumerate()
            .any(|(i, f)| *f && i != self.cur);
        if !busy {
            let stale: Vec<_> = self
                .tex_cache
                .keys()
                .filter(|id| !live.contains(*id))
                .cloned()
                .collect();
            for id in stale {
                if let Some((_, tex)) = self.tex_cache.remove(&id) {
                    let bucket = self.tex_pool.entry((tex.w, tex.h)).or_default();
                    if bucket.len() < 6 {
                        bucket.push(tex);
                    } else {
                        tex.destroy(&self.ctx.device, self.arena.as_mut());
                    }
                }
            }
            // Imported textures destroy instead of pooling —
            // their memory belongs to one specific dmabuf.
            let stale_imp: Vec<_> = self
                .imp_cache
                .keys()
                .filter(|id| !live.contains(*id))
                .cloned()
                .collect();
            for id in stale_imp {
                if let Some(it) = self.imp_cache.remove(&id) {
                    it.tex.destroy(&self.ctx.device, None);
                }
            }
            // Failed-import marks die with their buffers — a
            // recycled id must get a fresh import attempt.
            self.imp_bad.retain(|id| live.contains(id));
            // Deferred retire — textures replaced this frame could
            // still be referenced by an in-flight submission's
            // descriptor set until the older parities retired.
            for tex in self.tex_retire.drain(..) {
                let bucket = self.tex_pool.entry((tex.w, tex.h)).or_default();
                if bucket.len() < 6 {
                    bucket.push(tex);
                } else {
                    tex.destroy(&self.ctx.device, self.arena.as_mut());
                }
            }
            for tex in self.imp_retire.drain(..) {
                tex.destroy(&self.ctx.device, None);
            }
        }
        // The presented parity is the newest one this frame's
        // retirement scan proved complete — never `cur`, whose
        // submission is still executing. With nothing retired
        // this pass (e.g. settle already claimed it), any older
        // complete parity is a safe present.
        let out = if self.first_present {
            self.first_present = false;
            self.cur
        } else {
            retired_idx
                .or(self.last_out)
                .unwrap_or((self.cur + 1) % super::PARITIES)
        };
        self.cur = (self.cur + 1) % super::PARITIES;
        self.last_out = Some(out);
        Ok(unsafe {
            std::slice::from_raw_parts(self.target.readbacks[out].ptr, (w * h * 4) as usize)
        })
    }
}
