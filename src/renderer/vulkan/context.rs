// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Vulkan device context — instance, physical device matched to
//! the compositor's DRM node, logical device, queue and command
//! pool. Everything downstream (pipelines, images, targets) hangs
//! off this; nothing here knows about surfaces or scene content.

use anyhow::{Context, Result};
use ash::vk;
use std::ffi::CStr;
use std::os::fd::RawFd;

/// Instance extension needed to query VkPhysicalDeviceDrmPropertiesEXT.
const KHR_GET_PHYSICAL_DEV_PROPS2: &CStr = c"VK_KHR_get_physical_device_properties2";
/// Lets us bind a physical device to a specific /dev/dri node —
/// optional, absent on some drivers; matching falls back then.
const EXT_PHYSICAL_DEVICE_DRM: &CStr = c"VK_EXT_physical_device_drm";
/// Device extensions for the DMABUF export/zero-copy target path.
const KHR_EXTERNAL_MEMORY_FD: &CStr = c"VK_KHR_external_memory_fd";
const EXT_EXTERNAL_MEMORY_DMA_BUF: &CStr = c"VK_EXT_external_memory_dma_buf";
const EXT_IMAGE_DRM_FORMAT_MODIFIER: &CStr = c"VK_EXT_image_drm_format_modifier";

/// Everything held by the backend — drop order is field order
/// (pool/device die before instance, entry last).
pub struct VkContext {
    /// RAII handle on the Vulkan library — dropping it while
    /// instances/devices live would invalidate them.
    #[allow(dead_code)]
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    /// Family index for later submits — the graphics queue
    /// handle alone is enough until a second queue lands.
    #[allow(dead_code)]
    pub queue_family: u32,
    pub queue: vk::Queue,
    pub cmd_pool: vk::CommandPool,
    /// Whether VK_EXT_image_drm_format_modifier is present — gates
    /// the zero-copy DMABUF framebuffer path.
    pub has_modifier_ext: bool,
    /// Whether VK_EXT_external_memory_dma_buf is present — gates
    /// dmabuf export of the readback memory for direct scanout.
    pub has_dma_buf_ext: bool,
    /// Memory-type table for allocations.
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    /// `minUniformBufferOffsetAlignment` — stride for the drain's
    /// dynamic-offset parameter slots.
    pub ubo_align: u64,
}

/// Instance + matched physical device — the shared front half
/// of `VkContext::new`, also used by the early modifier probe
/// that must run before any client binds the dmabuf global.
fn open_instance(card_fd: RawFd) -> Result<(ash::Entry, ash::Instance, vk::PhysicalDevice)> {
    // SAFETY: libvulkan must exist — checked by the error path.
    let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;

    // Only request instance extensions the loader reports —
    // asking for absent ones is a hard error on some drivers.
    let sup: Vec<vk::ExtensionProperties> =
        unsafe { entry.enumerate_instance_extension_properties(None)? };
    let have = |name: &CStr| {
        sup.iter().any(|p| {
            let n = unsafe { CStr::from_ptr(p.extension_name.as_ptr()) };
            n == name
        })
    };
    let mut inst_exts: Vec<*const i8> = Vec::new();
    if have(KHR_GET_PHYSICAL_DEV_PROPS2) {
        inst_exts.push(KHR_GET_PHYSICAL_DEV_PROPS2.as_ptr());
    }
    let has_drm_props = have(EXT_PHYSICAL_DEVICE_DRM);
    if has_drm_props {
        inst_exts.push(EXT_PHYSICAL_DEVICE_DRM.as_ptr());
    }
    let app = vk::ApplicationInfo::default()
        .application_name(c"odin")
        .api_version(vk::API_VERSION_1_2);
    let ici = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&inst_exts);
    let instance =
        unsafe { entry.create_instance(&ici, None) }.context("vkCreateInstance failed")?;

    // Match the physical device to OUR drm node — dev_t of the
    // card fd vs the device's primary/render node ids.
    let st = rustix::fs::fstat(unsafe { std::os::fd::BorrowedFd::borrow_raw(card_fd) })
        .context("fstat(drm fd)")?;
    let (want_major, want_minor) = (
        libc::major(st.st_rdev) as i64,
        libc::minor(st.st_rdev) as i64,
    );
    // A device is usable only with a graphics-capable queue —
    // check that first so the fallback never lands on a headless
    // compute-only device.
    let graphics_family = |pd: vk::PhysicalDevice| -> Option<u32> {
        unsafe { instance.get_physical_device_queue_family_properties(pd) }
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .map(|i| i as u32)
    };
    let pds = unsafe { instance.enumerate_physical_devices()? };
    let mut physical = None;
    if has_drm_props {
        for pd in &pds {
            let (hit, name) = {
                let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                unsafe { instance.get_physical_device_properties2(*pd, &mut props2) };
                // Last props2 use FIRST — its &mut borrow of `drm`
                // ends here, freeing `drm` for the field reads.
                let name = unsafe { CStr::from_ptr(props2.properties.device_name.as_ptr()) }
                    .to_str()
                    .unwrap_or("?")
                    .to_string();
                let primary_hit = drm.has_primary == vk::TRUE
                    && drm.primary_major == want_major
                    && drm.primary_minor == want_minor;
                let render_hit = drm.has_render == vk::TRUE
                    && drm.render_major == want_major
                    && drm.render_minor == want_minor;
                (primary_hit || render_hit, name)
            };
            if hit && graphics_family(*pd).is_some() {
                physical = Some(*pd);
                tracing::info!(name, "vulkan: physical device matched drm node");
                break;
            }
        }
    }
    let physical = match physical {
        Some(p) => p,
        // No DRM match (or extension absent) — first device with
        // a graphics queue. Single-GPU systems land correctly;
        // exotic multi-GPU boxes get a working device rather
        // than a failure.
        None => pds
            .iter()
            .copied()
            .find(|pd| graphics_family(*pd).is_some())
            .context("vulkan: no graphics-capable physical device")?,
    };
    Ok((entry, instance, physical))
}

impl VkContext {
    /// The DRM modifiers this physical device can SAMPLE in
    /// B8G8R8A8 — the dmabuf global advertises exactly this set.
    /// Empty when the driver lacks the modifier extension.
    fn sampled_modifiers(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Vec<u64> {
        let mut mod_list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut mod_list);
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut fp2,
            )
        };
        let mut props = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            mod_list.drm_format_modifier_count as usize
        ];
        let mut mod_list2 = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut props);
        let mut fp3 = vk::FormatProperties2::default().push_next(&mut mod_list2);
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut fp3,
            )
        };
        props
            .iter()
            .filter(|p| {
                p.drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|p| p.drm_format_modifier)
            .collect()
    }

    /// Early capability probe — runs before XWayland spawns so
    /// the dmabuf global advertises real GPU modifiers at first
    /// bind. Instance + physical device only, ~ms-cost; errors
    /// yield an empty list (LINEAR-only advertisement).
    pub fn probe_modifiers(card_fd: RawFd) -> Vec<u64> {
        let mods = (|| {
            let (entry, instance, physical) = open_instance(card_fd).ok()?;
            let dev_sup = unsafe {
                instance
                    .enumerate_device_extension_properties(physical)
                    .ok()?
            };
            let has_mod = dev_sup.iter().any(|p| {
                let n = unsafe { CStr::from_ptr(p.extension_name.as_ptr()) };
                n == EXT_IMAGE_DRM_FORMAT_MODIFIER
            });
            let mods = if has_mod {
                Self::sampled_modifiers(&instance, physical)
            } else {
                Vec::new()
            };
            unsafe { instance.destroy_instance(None) };
            drop(entry);
            Some(mods)
        })()
        .unwrap_or_default();
        if !mods.is_empty() {
            crate::compositor::dmabuf::set_advertised_modifiers(mods.clone());
        }
        mods
    }

    pub fn new(card_fd: RawFd) -> Result<Self> {
        let (entry, instance, physical) = open_instance(card_fd)?;

        // Graphics-capable queue family.
        let fams = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = fams
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .context("vulkan: no graphics queue family")? as u32;

        // Device extensions — only what's offered.
        let dev_sup = unsafe { instance.enumerate_device_extension_properties(physical)? };
        let dhave = |name: &CStr| {
            dev_sup.iter().any(|p| {
                let n = unsafe { CStr::from_ptr(p.extension_name.as_ptr()) };
                n == name
            })
        };
        let mut dev_exts: Vec<*const i8> = Vec::new();
        let has_dma_buf_ext = dhave(EXT_EXTERNAL_MEMORY_DMA_BUF);
        for n in [KHR_EXTERNAL_MEMORY_FD, EXT_EXTERNAL_MEMORY_DMA_BUF] {
            if dhave(n) {
                dev_exts.push(n.as_ptr());
            }
        }
        let has_modifier_ext = dhave(EXT_IMAGE_DRM_FORMAT_MODIFIER);
        if has_modifier_ext {
            dev_exts.push(EXT_IMAGE_DRM_FORMAT_MODIFIER.as_ptr());
        }
        // Republish the modifier set — the early probe usually
        // already did this, but a session that never probed (or
        // probed a different card) still gets the real list.
        if has_modifier_ext {
            let mods = Self::sampled_modifiers(&instance, physical);
            tracing::info!(count = mods.len(), "vulkan: sampled dma modifiers");
            crate::compositor::dmabuf::set_advertised_modifiers(mods);
        }

        let prio = [1.0f32];
        let qinfo = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&prio)];
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qinfo)
            .enabled_extension_names(&dev_exts);
        let device = unsafe { instance.create_device(physical, &dci, None) }
            .context("vkCreateDevice failed")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .context("vkCreateCommandPool failed")?;
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let ubo_align = unsafe {
            instance
                .get_physical_device_properties(physical)
                .limits
                .min_uniform_buffer_offset_alignment
        }
        .max(16);
        Ok(Self {
            entry,
            instance,
            physical,
            device,
            queue_family,
            queue,
            cmd_pool,
            has_modifier_ext,
            has_dma_buf_ext,
            mem_props,
            ubo_align,
        })
    }
}

impl Drop for VkContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
