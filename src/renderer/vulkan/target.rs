// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-output target — the image the scene is rendered into, its
//! framebuffer for the render pass, a host-visible readback buffer
//! that carries the finished frame to the DRM dumb buffer, plus
//! the command buffer and fence that pace it.

use anyhow::{Context, Result};
use ash::vk;

/// First memory type satisfying `flags` inside `allowed` — the
/// standard selection helper.
pub fn mem_type(
    mem: &vk::PhysicalDeviceMemoryProperties,
    allowed: u32,
    flags: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..mem.memory_type_count)
        .find(|&i| {
            (allowed & (1 << i)) != 0 && mem.memory_types[i as usize].property_flags.contains(flags)
        })
        .context("no suitable vulkan memory type")
}

/// Image + bound memory + view — created per output target and per
/// surface upload (surface textures are v1-simple: created per
/// frame while fences pace reuse).
/// Shared host-visible memory arena — ONE vkAllocateMemory
/// backing many LINEAR images via `vkBindImageMemory` offsets.
/// Per-surface memory allocations are the most expensive driver
/// call on several vendors (tens of ms each for sysmem pinning);
/// a client startup burst used to stall the loop ~100 ms per
/// window. First-fit over released ranges, bump pointer beyond.
pub struct Arena {
    pub(crate) mem: vk::DeviceMemory,
    ptr: *mut u8,
    ty: u32,
    cap: u64,
    next: u64,
    free: Vec<(u64, u64)>,
}

impl Arena {
    pub fn new(ctx: &super::context::VkContext, cap: u64) -> Result<Self> {
        let dev = &ctx.device;
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT
            | vk::MemoryPropertyFlags::HOST_CACHED;
        // Any allowed type — individual images check the bit.
        let ty = mem_type(&ctx.mem_props, u32::MAX, want).or_else(|_| {
            mem_type(
                &ctx.mem_props,
                u32::MAX,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        })?;
        let mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(cap)
                    .memory_type_index(ty),
                None,
            )
        }
        .context("arena vkAllocateMemory")?;
        let ptr = unsafe { dev.map_memory(mem, 0, cap, vk::MemoryMapFlags::empty()) }
            .context("arena vkMapMemory")? as *mut u8;
        Ok(Self {
            mem,
            ptr,
            ty,
            cap,
            next: 0,
            free: Vec::new(),
        })
    }

    /// Reserve `reqs.size` bytes at `reqs.alignment` — first-fit
    /// over released ranges, bump-allocated beyond. `None` means
    /// the arena is full or the image rejects this memory type;
    /// the caller then takes a dedicated allocation instead.
    fn slot(&mut self, reqs: &vk::MemoryRequirements) -> Option<u64> {
        if reqs.memory_type_bits & (1 << self.ty) == 0 {
            return None;
        }
        let al = reqs.alignment.max(1);
        if let Some(i) = self.free.iter().position(|&(off, sz)| {
            let a = off.div_ceil(al) * al;
            a + reqs.size <= off + sz
        }) {
            let (off, sz) = self.free.remove(i);
            let a = off.div_ceil(al) * al;
            let end = a + reqs.size;
            if a > off {
                self.free.push((off, a - off));
            }
            if end < off + sz {
                self.free.push((end, off + sz - end));
            }
            return Some(a);
        }
        let a = self.next.div_ceil(al) * al;
        if a + reqs.size > self.cap {
            return None;
        }
        self.next = a + reqs.size;
        Some(a)
    }

    /// Give a slot back — adjacent ranges are merged so long
    /// sessions don't fragment the bump watermark's front.
    pub fn release(&mut self, off: u64, size: u64) {
        self.free.push((off, size));
        self.free.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free.len());
        for (o, s) in self.free.drain(..) {
            match merged.last_mut() {
                Some((po, ps)) if *po + *ps == o => *ps += s,
                _ => merged.push((o, s)),
            }
        }
        self.free = merged;
    }

    /// Memory handle plus CPU base — images bind `mem` at their
    /// slot offset; `ptr` already points at mapped bytes.
    fn bind_parts(&self, off: u64) -> (vk::DeviceMemory, *mut u8) {
        (self.mem, unsafe { self.ptr.offset(off as isize) })
    }
}

pub struct Tex {
    pub image: vk::Image,
    pub mem: vk::DeviceMemory,
    pub view: vk::ImageView,
    /// Image extent — kept for the scissor/upload paths the
    /// GPU renderer will need.
    #[allow(dead_code)]
    pub w: u32,
    #[allow(dead_code)]
    pub h: u32,
    /// Mapped base pointer and row pitch when the image lives in
    /// host-visible memory — null/0 for device-local images.
    pub ptr: *mut u8,
    pub pitch: usize,
    /// Arena reservation (offset, size) when this image is bound
    /// into shared host memory — `destroy` hands the slot back
    /// instead of unmapping and freeing a private allocation.
    slot: Option<(u64, u64)>,
}

impl Tex {
    /// Allocate a sampled image — for render targets pass
    /// `| TRANSFER_SRC`, for upload textures
    /// `TRANSFER_DST | SAMPLED`.
    /// Shared image creation — tiling picked by `host`.
    fn make(
        ctx: &super::context::VkContext,
        w: u32,
        h: u32,
        usage: vk::ImageUsageFlags,
        host: bool,
        arena: Option<&mut Arena>,
    ) -> Result<Self> {
        let dev = &ctx.device;
        if host {
            // A host-visible image is only legal when the format's
            // LINEAR tiling supports every requested usage.
            let fp = unsafe {
                ctx.instance
                    .get_physical_device_format_properties(ctx.physical, vk::Format::B8G8R8A8_UNORM)
            };
            // Map each requested usage to its format feature and
            // require all of them for LINEAR tiling.
            let mut need = vk::FormatFeatureFlags::empty();
            for (u, f) in [
                (
                    vk::ImageUsageFlags::SAMPLED,
                    vk::FormatFeatureFlags::SAMPLED_IMAGE,
                ),
                (
                    vk::ImageUsageFlags::COLOR_ATTACHMENT,
                    vk::FormatFeatureFlags::COLOR_ATTACHMENT,
                ),
                (
                    vk::ImageUsageFlags::TRANSFER_SRC,
                    vk::FormatFeatureFlags::TRANSFER_SRC,
                ),
                (
                    vk::ImageUsageFlags::TRANSFER_DST,
                    vk::FormatFeatureFlags::TRANSFER_DST,
                ),
            ] {
                if usage.contains(u) {
                    need |= f;
                }
            }
            if !fp.linear_tiling_features.contains(need) {
                anyhow::bail!("linear tiling lacks required features {need:?}");
            }
        }
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
                    .tiling(if host {
                        vk::ImageTiling::LINEAR
                    } else {
                        vk::ImageTiling::OPTIMAL
                    })
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .context("vkCreateImage")?;
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        // Host images prefer a shared arena slot — binding into a
        // pre-mapped allocation skips vkAllocateMemory+vkMapMemory,
        // the two calls that dominate per-surface upload cost on
        // drivers with slow sysmem pinning. Slot = (offset, size)
        // returned to the arena on destroy.
        let mut slot = None;
        let mut arena_base: Option<*mut u8> = None;
        let mem = if host {
            let picked = arena.and_then(|ar| {
                ar.slot(&reqs).map(|off| {
                    let (m, p) = ar.bind_parts(off);
                    (off, m, p)
                })
            });
            match picked {
                Some((off, m, p)) => {
                    slot = Some((off, reqs.size));
                    arena_base = Some(p);
                    unsafe { dev.bind_image_memory(image, m, off) }?;
                    m
                }
                None => {
                    let ty = mem_type(
                        &ctx.mem_props,
                        reqs.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?;
                    let m = unsafe {
                        dev.allocate_memory(
                            &vk::MemoryAllocateInfo::default()
                                .allocation_size(reqs.size)
                                .memory_type_index(ty),
                            None,
                        )
                    }
                    .context("vkAllocateMemory")?;
                    unsafe { dev.bind_image_memory(image, m, 0) }?;
                    m
                }
            }
        } else {
            let ty = mem_type(
                &ctx.mem_props,
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            let m = unsafe {
                dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(ty),
                    None,
                )
            }
            .context("vkAllocateMemory")?;
            unsafe { dev.bind_image_memory(image, m, 0) }?;
            m
        };
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
        .context("vkCreateImageView")?;
        let (ptr, pitch) = if host {
            let lay = unsafe {
                dev.get_image_subresource_layout(
                    image,
                    vk::ImageSubresource {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        array_layer: 0,
                    },
                )
            };
            // Arena slots share the arena's permanent mapping —
            // only dedicated allocations need their own map.
            let p = match arena_base {
                Some(p) => p,
                None => unsafe {
                    dev.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                        .context("vkMapMemory")? as *mut u8
                },
            };
            (p, lay.row_pitch as usize)
        } else {
            (std::ptr::null_mut(), 0)
        };
        Ok(Self {
            image,
            mem,
            view,
            w,
            h,
            ptr,
            pitch,
            slot,
        })
    }

    pub fn new(
        ctx: &super::context::VkContext,
        w: u32,
        h: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Self> {
        Self::make(ctx, w, h, usage, false, None)
    }

    /// Host-visible LINEAR image — CPU memcpy in/out, no copy
    /// engine involved at all. The NVIDIA CE sysmem path faults on
    /// some driver releases (Xid 31), so uploads and readback both
    /// go through this path: the graphics engine does the GPU work,
    /// the CPU does the memory moves.
    pub fn new_host(
        ctx: &super::context::VkContext,
        w: u32,
        h: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Self> {
        Self::make(ctx, w, h, usage, true, None)
    }

    /// Host image bound into the shared arena — skips
    /// vkAllocateMemory/vkMapMemory entirely; falls back to a
    /// dedicated allocation when the slot can't be served.
    pub fn new_host_in(
        ctx: &super::context::VkContext,
        arena: &mut Arena,
        w: u32,
        h: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Self> {
        Self::make(ctx, w, h, usage, true, Some(arena))
    }

    /// Wrap an externally imported image+memory — the dmabuf
    /// path builds its VkImage against the client's fd, so the
    /// allocation is dedicated (never arena, never pooled).
    /// `destroy` frees the imported memory normally.
    pub fn imported(
        image: vk::Image,
        mem: vk::DeviceMemory,
        view: vk::ImageView,
        w: u32,
        h: u32,
    ) -> Self {
        Self {
            image,
            mem,
            view,
            w,
            h,
            ptr: std::ptr::null_mut(),
            pitch: 0,
            slot: None,
        }
    }

    /// Destroy the image and its view — arena-bound textures
    /// return their slot to `arena` (which must be the same one
    /// the image was allocated from); dedicated textures unmap
    /// and free their own memory.
    pub fn destroy(&self, dev: &ash::Device, mut arena: Option<&mut Arena>) {
        unsafe {
            dev.destroy_image_view(self.view, None);
            dev.destroy_image(self.image, None);
        }
        match self.slot {
            Some((off, size)) => {
                if let Some(ar) = arena.as_deref_mut() {
                    ar.release(off, size);
                }
            }
            None => unsafe {
                if !self.ptr.is_null() {
                    dev.unmap_memory(self.mem);
                }
                dev.free_memory(self.mem, None);
            },
        }
    }
}

/// Host-visible buffer — upload staging for textures, download
/// staging for the finished frame.
pub struct HostBuf {
    pub buf: vk::Buffer,
    pub mem: vk::DeviceMemory,
    pub ptr: *mut u8,
    /// Allocation size — kept for bounds-checking copy paths.
    #[allow(dead_code)]
    pub size: usize,
}

impl HostBuf {
    pub fn new(
        ctx: &super::context::VkContext,
        size: usize,
        usage: vk::BufferUsageFlags,
    ) -> Result<Self> {
        Self::with_flags(
            ctx,
            size,
            usage,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
    }

    pub(crate) fn with_flags(
        ctx: &super::context::VkContext,
        size: usize,
        usage: vk::BufferUsageFlags,
        want: vk::MemoryPropertyFlags,
    ) -> Result<Self> {
        Self::alloc(ctx, size, usage, want)
    }

    /// Host-mapped allocation — `want` is the memory property
    /// mask (e.g. HOST_CACHED for CPU-read buffers); the mapping
    /// pointer is null on device-local-only types.
    pub(crate) fn alloc(
        ctx: &super::context::VkContext,
        size: usize,
        usage: vk::BufferUsageFlags,
        want: vk::MemoryPropertyFlags,
    ) -> Result<Self> {
        let dev = &ctx.device;
        let buf = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size as u64)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .context("vkCreateBuffer")?;
        let reqs = unsafe { dev.get_buffer_memory_requirements(buf) };
        let ty = mem_type(&ctx.mem_props, reqs.memory_type_bits, want)?;
        let mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(ty),
                None,
            )
        }?;
        unsafe { dev.bind_buffer_memory(buf, mem, 0) }?;
        let ty_props = ctx.mem_props.memory_types[ty as usize].property_flags;
        let ptr = if ty_props.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            unsafe { dev.map_memory(mem, 0, size as u64, vk::MemoryMapFlags::empty()) }
                .context("vkMapMemory")? as *mut u8
        } else {
            std::ptr::null_mut()
        };
        Ok(Self {
            buf,
            mem,
            ptr,
            size,
        })
    }

    pub fn destroy(&self, dev: &ash::Device) {
        unsafe {
            dev.unmap_memory(self.mem);
            dev.destroy_buffer(self.buf, None);
            dev.free_memory(self.mem, None);
        }
    }
}

/// Everything `render()` needs per frame — built once in `new`.
pub struct Target {
    /// Device-local color attachment — the drain copies it into
    /// one of the BAR-window `readbacks` the CPU then reads.
    /// Copy-engine traffic never touches sysmem this way.
    pub color: Tex,
    pub fb: vk::Framebuffer,
    /// BAR-window readbacks the CPU then reads — double-buffered:
    /// the CPU gets the previous frame's completed parity while
    /// the GPU drains into the other, so submit never stalls.
    pub readbacks: [HostBuf; 3],
    /// One command buffer per parity — a frame records into the
    /// parity it will render, while earlier parities may still be
    /// executing on the GPU.
    pub cmds: [vk::CommandBuffer; 3],
    /// One fence per parity — render waits on the parity it is
    /// about to overwrite (3 submissions old ⇒ already signaled),
    /// so the CPU never serializes on the previous frame's GPU
    /// work. Present retires whichever in-flight parity signaled.
    pub fences: [vk::Fence; 3],
    /// One descriptor pool per parity — resetting a pool frees
    /// every set in it, so per-frame draw descriptors must not
    /// share a pool with still-in-flight submissions.
    pub ds_pools: [vk::DescriptorPool; 3],
}

impl Target {
    pub fn new(
        ctx: &super::context::VkContext,
        pipe: &super::pipeline::Pipe,
        w: u32,
        h: u32,
    ) -> Result<Self> {
        let dev = &ctx.device;
        let color = Tex::new(
            ctx,
            w,
            h,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
        )?;
        let fb = unsafe {
            dev.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(pipe.render_pass)
                    .attachments(std::slice::from_ref(&color.view))
                    .width(w)
                    .height(h)
                    .layers(1),
                None,
            )
        }
        .context("vkCreateFramebuffer")?;
        // Cached sysmem — the drain writes it on the graphics
        // engine, cached reads keep the dumb-buffer copy fast.
        // Plain HOST_COHERENT sysmem is write-combined on
        // discrete GPUs: CPU reads of a full frame take ~100ms+.
        let make_readback = || -> Result<HostBuf> {
            HostBuf::with_flags(
                ctx,
                (w * h * 4) as usize,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                vk::MemoryPropertyFlags::HOST_VISIBLE
                    | vk::MemoryPropertyFlags::HOST_COHERENT
                    | vk::MemoryPropertyFlags::HOST_CACHED,
            )
            .or_else(|_| {
                HostBuf::new(
                    ctx,
                    (w * h * 4) as usize,
                    vk::BufferUsageFlags::STORAGE_BUFFER,
                )
            })
        };
        let readbacks = [make_readback()?, make_readback()?, make_readback()?];
        let cmd_bufs = unsafe {
            dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(ctx.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(3),
            )
        }
        .context("vkAllocateCommandBuffers")?;
        let cmds: [vk::CommandBuffer; 3] = [cmd_bufs[0], cmd_bufs[1], cmd_bufs[2]];
        // SIGNALED at creation — the first wait on each parity
        // must pass immediately instead of deadlocking on a fence
        // that was never submitted to.
        let mut fences = [vk::Fence::null(); 3];
        for f in &mut fences {
            *f = unsafe {
                dev.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }
            .context("vkCreateFence")?;
        }
        // Per-parity descriptor pools — same shape as the shared
        // one the pipe builds, but owned by Target so a pool reset
        // only ever kills the parity's own retired descriptors.
        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                .descriptor_count(64),
        ];
        let mut ds_pools = [vk::DescriptorPool::null(); 3];
        for p in &mut ds_pools {
            *p = unsafe {
                dev.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(256)
                        .pool_sizes(&sizes),
                    None,
                )
            }
            .context("per-parity vkCreateDescriptorPool")?;
        }
        Ok(Self {
            color,
            fb,
            readbacks,
            cmds,
            fences,
            ds_pools,
        })
    }

    pub fn destroy(&self, dev: &ash::Device) {
        unsafe {
            for f in &self.fences {
                dev.destroy_fence(*f, None);
            }
            for p in &self.ds_pools {
                dev.destroy_descriptor_pool(*p, None);
            }
            dev.destroy_framebuffer(self.fb, None);
        }
        for rb in &self.readbacks {
            rb.destroy(dev);
        }
        // fds die with their OwnedFd wrappers.
        self.color.destroy(dev, None);
    }
}
