//! The only GPU work: one compute dispatch per recorded frame that reads the
//! panel's scanout buffer in place and writes NV12 into the encoder's input
//! buffer. Both buffers are borrowed as dmabufs, nothing is copied.
//!
//! The queue asks for low priority so the game and the compositor always go
//! first; the recorder's frame can wait a millisecond, theirs can't.

use std::ffi::CStr;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

use anyhow::{bail, Context, Result};
use ash::vk;

use crate::grab::Exported;
use crate::lens::{Grid, STEP};

const SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/convert.spv"));
const DMA_BUF: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
/// Swapchain images the compositor cycles through, plus spares for resizes.
const MAX_SCANOUTS: usize = 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    out_w: u32,
    out_h: u32,
    luma_stride: u32,
    chroma_stride: u32,
    grid_w: u32,
    step: u32,
    per_channel: u32,
    luma_offset: u32,
    chroma_offset: u32,
}

/// One encoder input buffer as the encoder lays it out.
pub struct TargetPlanes {
    pub luma: OwnedFd,
    pub luma_offset: u32,
    pub luma_stride: u32,
    pub chroma: OwnedFd,
    pub chroma_offset: u32,
    pub chroma_stride: u32,
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

struct Target {
    /// Borrowed from the encoder (false: the GPU's own memory, for tests).
    external: bool,
    luma: Buffer,
    chroma: Buffer,
    luma_offset: u32,
    luma_stride: u32,
    chroma_offset: u32,
    chroma_stride: u32,
}

struct Scanout {
    /// Borrowed from the display (false: the GPU's own image, for tests).
    external: bool,
    id: u64,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
}

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    phys: vk::PhysicalDevice,
    device: ash::Device,
    memory_fd: ash::khr::external_memory_fd::Device,
    foreign: u32,
    family: u32,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptors: vk::DescriptorPool,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    grid: Buffer,
    grid_w: u32,
    per_channel: bool,
    out: (u32, u32),
    scanouts: Vec<Scanout>,
    targets: Vec<Target>,
    /// The conversion in flight: its encoder buffer and when it was submitted.
    pending: Option<(usize, std::time::Instant)>,
    gpu_us: Vec<u64>,
    /// Whether the queue got the low priority it asked for.
    pub low_priority: bool,
    modifiers: bool,
}

impl Gpu {
    /// Sets up the device and the conversion for one output size and grid.
    pub fn new(grid: &Grid, out_w: u32, out_h: u32) -> Result<Self> {
        Self::with_priority(grid, out_w, out_h, std::env::var_os("SFRECORDER_NORMAL_PRIORITY").is_none())
    }

    /// As `new`, choosing whether to ask for a low-priority queue.
    pub fn with_priority(grid: &Grid, out_w: u32, out_h: u32, wants_low: bool) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("no Vulkan driver")?;
        let app = vk::ApplicationInfo::default().application_name(c"sfrecorder").api_version(vk::API_VERSION_1_1);
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }?;

        let required = [ash::khr::external_memory_fd::NAME, ash::ext::external_memory_dma_buf::NAME];
        let (phys, available) = unsafe { instance.enumerate_physical_devices()? }
            .into_iter()
            .find_map(|p| {
                let exts: Vec<_> = unsafe { instance.enumerate_device_extension_properties(p).ok()? }
                    .iter()
                    .map(|e| e.extension_name_as_c_str().unwrap_or_default().to_owned())
                    .collect();
                required.iter().all(|r| exts.iter().any(|e| e.as_c_str() == *r)).then_some((p, exts))
            })
            .context("the GPU driver can't borrow display buffers (dmabuf import missing)")?;
        let has = |name: &CStr| available.iter().any(|e| e.as_c_str() == name);

        let family = unsafe { instance.get_physical_device_queue_family_properties(phys) }
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .context("no compute queue")? as u32;

        let mut extensions: Vec<*const std::ffi::c_char> = required.iter().map(|n| n.as_ptr()).collect();
        // Needed to read the display's (tiled, compressed) buffers; only a
        // test driver lacks it, and then only display buffers can't be read.
        let modifiers = has(ash::ext::image_drm_format_modifier::NAME);
        if modifiers {
            extensions.push(ash::ext::image_drm_format_modifier::NAME.as_ptr());
        }
        // The modifier extension depends on these on 1.1 drivers.
        for dep in [ash::khr::image_format_list::NAME, ash::khr::bind_memory2::NAME, ash::khr::sampler_ycbcr_conversion::NAME] {
            if has(dep) {
                extensions.push(dep.as_ptr());
            }
        }
        let foreign = if has(ash::ext::queue_family_foreign::NAME) {
            extensions.push(ash::ext::queue_family_foreign::NAME.as_ptr());
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            vk::QUEUE_FAMILY_EXTERNAL
        };
        let mut priority = vk::DeviceQueueGlobalPriorityCreateInfoKHR::default().global_priority(vk::QueueGlobalPriorityKHR::LOW);
        let low_priority = if !wants_low {
            false
        } else if has(ash::khr::global_priority::NAME) {
            extensions.push(ash::khr::global_priority::NAME.as_ptr());
            true
        } else if has(ash::ext::global_priority::NAME) {
            extensions.push(ash::ext::global_priority::NAME.as_ptr());
            true
        } else {
            false
        };
        let priorities = [0.0f32];
        let mut queue_info = vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priorities);
        if low_priority {
            queue_info = queue_info.push_next(&mut priority);
        }
        let queues = [queue_info];
        // Out-of-range writes are dropped instead of faulting the GPU, in case
        // an encoder lays its buffer out differently than it says.
        let supported = unsafe { instance.get_physical_device_features(phys) };
        let features = vk::PhysicalDeviceFeatures::default().robust_buffer_access(supported.robust_buffer_access == vk::TRUE);
        let device = unsafe {
            instance.create_device(phys, &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&extensions).enabled_features(&features), None)
        }
            .context("creating the Vulkan device")?;
        let memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(family, 0) };

        unsafe {
            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)?;
            let cmd = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;

            let bindings = [
                binding(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
                binding(1, vk::DescriptorType::STORAGE_BUFFER),
                binding(2, vk::DescriptorType::STORAGE_BUFFER),
                binding(3, vk::DescriptorType::STORAGE_BUFFER),
            ];
            let set_layout = device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)?;
            let ranges = [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::COMPUTE).size(size_of::<Params>() as u32)];
            let set_layouts = [set_layout];
            let layout = device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts).push_constant_ranges(&ranges), None)?;
            let code: Vec<u32> = SHADER.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            let module = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
            let stage = vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module).name(c"main");
            let pipeline = device
                .create_compute_pipelines(vk::PipelineCache::null(), &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout)], None)
                .map_err(|(_, e)| e)?[0];
            device.destroy_shader_module(module, None);

            let sizes = [
                vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 1 },
                vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: 3 },
            ];
            let descriptors = device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes), None)?;
            let set = device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(descriptors).set_layouts(&set_layouts))?[0];
            let sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;

            let mut gpu = Self {
                _entry: entry,
                instance,
                phys,
                device,
                memory_fd,
                foreign,
                family,
                queue,
                pool,
                cmd,
                fence,
                set_layout,
                layout,
                pipeline,
                descriptors,
                set,
                sampler,
                grid: Buffer { buffer: vk::Buffer::null(), memory: vk::DeviceMemory::null() },
                grid_w: grid.width,
                per_channel: grid.per_channel,
                out: (out_w, out_h),
                scanouts: Vec::new(),
                targets: Vec::new(),
                pending: None,
                gpu_us: Vec::new(),
                low_priority,
                modifiers,
            };
            gpu.grid = gpu.upload(bytes_of(&grid.points))?;
            Ok(gpu)
        }
    }

    /// Borrows one of the encoder's input buffers; returns its index.
    pub fn add_target(&mut self, planes: TargetPlanes) -> Result<usize> {
        let luma = self.import_buffer(planes.luma)?;
        let chroma = self.import_buffer(planes.chroma)?;
        self.targets.push(Target {
            external: true,
            luma,
            chroma,
            luma_offset: planes.luma_offset,
            luma_stride: planes.luma_stride,
            chroma_offset: planes.chroma_offset,
            chroma_stride: planes.chroma_stride,
        });
        Ok(self.targets.len() - 1)
    }

    /// A stand-in for a display buffer, in the GPU's own memory (for tests):
    /// RGBA8 pixels, uploaded once.
    pub fn add_test_scanout(&mut self, id: u64, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let d = &self.device;
        let staging = self.upload(rgba)?;
        unsafe {
            let format = vk::Format::R8G8B8A8_UNORM;
            let image = d.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D { width, height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST),
                None,
            )?;
            let reqs = d.get_image_memory_requirements(image);
            let types = self.instance.get_physical_device_memory_properties(self.phys);
            let index = (0..types.memory_type_count).find(|&i| reqs.memory_type_bits & (1 << i) != 0).context("no memory for the test image")?;
            let memory = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(index), None)?;
            d.bind_image_memory(image, memory, 0)?;
            let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
            d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .image(image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(self.cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            d.cmd_copy_buffer_to_image(self.cmd, staging.buffer, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
            let to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .image(image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(self.cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[], &[to_general]);
            d.end_command_buffer(self.cmd)?;
            let cmds = [self.cmd];
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], self.fence)?;
            d.wait_for_fences(&[self.fence], true, u64::MAX)?;
            d.reset_fences(&[self.fence])?;
            d.destroy_buffer(staging.buffer, None);
            d.free_memory(staging.memory, None);
            let view = d.create_image_view(
                &vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(format).subresource_range(range),
                None,
            )?;
            self.scanouts.push(Scanout { external: false, id, image, view, memory });
        }
        Ok(())
    }

    /// A target in the GPU's own memory (for `--diagnose`): NV12 with the
    /// given strides, readable afterwards with `read_target`.
    pub fn add_host_target(&mut self, stride: u32, height: u32) -> Result<usize> {
        let luma = self.upload(&vec![0u8; (stride * height) as usize])?;
        let chroma = self.upload(&vec![0u8; (stride * height.div_ceil(2)) as usize])?;
        self.targets.push(Target { external: false, luma, chroma, luma_offset: 0, luma_stride: stride, chroma_offset: 0, chroma_stride: stride });
        Ok(self.targets.len() - 1)
    }

    /// The luma plane of a target made by `add_host_target`.
    pub fn read_luma(&self, target: usize, len: usize) -> Result<Vec<u8>> {
        let t = &self.targets[target];
        if t.external {
            bail!("only test targets can be read back");
        }
        unsafe {
            let ptr = self.device.map_memory(t.luma.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *const u8;
            let out = std::slice::from_raw_parts(ptr, len).to_vec();
            self.device.unmap_memory(t.luma.memory);
            Ok(out)
        }
    }

    /// Waits up to `timeout` for the running conversion; its time if it finished.
    pub fn wait(&mut self, timeout: std::time::Duration) -> Result<Option<std::time::Duration>> {
        let Some((_, since)) = self.pending else { bail!("nothing is running") };
        while since.elapsed() < timeout {
            if unsafe { self.device.get_fence_status(self.fence) }? {
                unsafe { self.device.reset_fences(&[self.fence]) }?;
                self.pending = None;
                return Ok(Some(since.elapsed()));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(None)
    }

    /// What the driver says it can do with this scanout layout.
    pub fn describe_scanout(&self, fourcc: u32, modifier: u64) -> String {
        let Some(format) = vk_format(fourcc) else { return format!("format {fourcc:#x}: not supported by SFRecorder") };
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { self.instance.get_physical_device_format_properties2(self.phys, format, &mut props) };
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { self.instance.get_physical_device_format_properties2(self.phys, format, &mut props) };
        match mods.iter().find(|m| m.drm_format_modifier == modifier) {
            None => format!("{format:?} modifier {modifier:#x}: NOT supported by the Vulkan driver ({} others are)", mods.len()),
            Some(m) => format!(
                "{format:?} modifier {modifier:#x}: {} plane(s), sampled {}, linear filter {}",
                m.drm_format_modifier_plane_count,
                m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE),
                m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR)
            ),
        }
    }

    /// True if this scanout buffer is already borrowed (no need to export it again).
    pub fn knows(&self, id: u64) -> bool {
        self.scanouts.iter().any(|s| s.id == id)
    }

    /// Borrows a scanout buffer, keeping the most recent few.
    pub fn add_scanout(&mut self, id: u64, buf: Exported) -> Result<()> {
        if self.scanouts.len() >= MAX_SCANOUTS {
            let old = self.scanouts.remove(0);
            unsafe {
                self.device.destroy_image_view(old.view, None);
                self.device.destroy_image(old.image, None);
                self.device.free_memory(old.memory, None);
            }
        }
        let scanout = self.import_image(id, buf)?;
        self.scanouts.push(scanout);
        Ok(())
    }

    /// True while a conversion is still running on the GPU.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// The encoder buffer of a finished conversion, if one just finished.
    /// The recorder never waits on the GPU: a conversion that isn't done yet
    /// is checked again at the next vblank, and frames are skipped meanwhile.
    pub fn finished(&mut self) -> Result<Option<usize>> {
        let Some((target, since)) = self.pending else { return Ok(None) };
        if unsafe { self.device.get_fence_status(self.fence) }? {
            unsafe { self.device.reset_fences(&[self.fence]) }?;
            self.pending = None;
            self.gpu_us.push(since.elapsed().as_micros() as u64);
            return Ok(Some(target));
        }
        if since.elapsed() > std::time::Duration::from_secs(2) {
            bail!("the GPU hasn't finished a frame in 2 s (it may have hit an error; see `sudo dmesg`)");
        }
        Ok(None)
    }

    /// Times from submit to done (µs) since the last call, for the log.
    pub fn take_timings(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.gpu_us)
    }

    /// Starts converting scanout `id` into encoder buffer `target`.
    pub fn convert(&mut self, id: u64, target: usize) -> Result<()> {
        if self.pending.is_some() {
            bail!("a conversion is already running");
        }
        let scanout = self.scanouts.iter().find(|s| s.id == id).context("unknown scanout buffer")?;
        let t = &self.targets[target];
        let d = &self.device;
        unsafe {
            let image_info = [vk::DescriptorImageInfo::default().sampler(self.sampler).image_view(scanout.view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let grid = [whole(self.grid.buffer)];
            let luma = [whole(t.luma.buffer)];
            let chroma = [whole(t.chroma.buffer)];
            let writes = [
                vk::WriteDescriptorSet::default().dst_set(self.set).dst_binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&image_info),
                vk::WriteDescriptorSet::default().dst_set(self.set).dst_binding(1).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&grid),
                vk::WriteDescriptorSet::default().dst_set(self.set).dst_binding(2).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&luma),
                vk::WriteDescriptorSet::default().dst_set(self.set).dst_binding(3).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&chroma),
            ];
            d.update_descriptor_sets(&writes, &[]);

            d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
            // Take the buffers from the display and the encoder...
            let image_in = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(if scanout.external { self.foreign } else { vk::QUEUE_FAMILY_IGNORED })
                .dst_queue_family_index(if scanout.external { self.family } else { vk::QUEUE_FAMILY_IGNORED })
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(scanout.image)
                .subresource_range(range);
            let external: &[vk::Buffer] = if t.external { &[t.luma.buffer, t.chroma.buffer] } else { &[] };
            let buffers_in: Vec<_> = external.iter().map(|&b| {
                vk::BufferMemoryBarrier::default()
                    .src_queue_family_index(self.foreign)
                    .dst_queue_family_index(self.family)
                    .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .buffer(b)
                    .size(vk::WHOLE_SIZE)
            }).collect();
            d.cmd_pipeline_barrier(self.cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::COMPUTE_SHADER, vk::DependencyFlags::empty(), &[], &buffers_in, &[image_in]);

            d.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(self.cmd, vk::PipelineBindPoint::COMPUTE, self.layout, 0, &[self.set], &[]);
            let params = Params {
                out_w: self.out.0,
                out_h: self.out.1,
                luma_stride: t.luma_stride,
                chroma_stride: t.chroma_stride,
                grid_w: self.grid_w,
                step: STEP,
                per_channel: self.per_channel as u32,
                luma_offset: t.luma_offset,
                chroma_offset: t.chroma_offset,
            };
            d.cmd_push_constants(self.cmd, self.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes_of(std::slice::from_ref(&params)));
            d.cmd_dispatch(self.cmd, self.out.0.div_ceil(4 * 8), self.out.1.div_ceil(2 * 8), 1);

            // ...and hand them back.
            let family = self.family;
            let image_out = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(if scanout.external { family } else { vk::QUEUE_FAMILY_IGNORED })
                .dst_queue_family_index(if scanout.external { self.foreign } else { vk::QUEUE_FAMILY_IGNORED })
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .image(scanout.image)
                .subresource_range(range);
            let buffers_out: Vec<_> = external.iter().map(|&b| {
                vk::BufferMemoryBarrier::default()
                    .src_queue_family_index(family)
                    .dst_queue_family_index(self.foreign)
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .buffer(b)
                    .size(vk::WHOLE_SIZE)
            }).collect();
            d.cmd_pipeline_barrier(self.cmd, vk::PipelineStageFlags::COMPUTE_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &buffers_out, &[image_out]);
            d.end_command_buffer(self.cmd)?;

            let cmds = [self.cmd];
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], self.fence)?;
        }
        self.pending = Some((target, std::time::Instant::now()));
        Ok(())
    }

    fn import_image(&self, id: u64, buf: Exported) -> Result<Scanout> {
        if !self.modifiers {
            bail!("the GPU driver can't read display buffers (no VK_EXT_image_drm_format_modifier)");
        }
        let format = vk_format(buf.fourcc).with_context(|| format!("unsupported panel format {:#x}", buf.fourcc))?;
        let d = &self.device;
        unsafe {
            let layouts = [vk::SubresourceLayout { offset: buf.offset as u64, size: 0, row_pitch: buf.pitch as u64, array_pitch: 0, depth_pitch: 0 }];
            let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(buf.modifier).plane_layouts(&layouts);
            let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: buf.width, height: buf.height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external)
                .push_next(&mut explicit);
            let image = d.create_image(&info, None).context("the GPU can't read the panel's buffer layout")?;
            let reqs = d.get_image_memory_requirements(image);
            let memory = match self.import_memory(buf.fd, reqs, Some(image), None) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_image(image, None);
                    return Err(e);
                }
            };
            d.bind_image_memory(image, memory, 0)?;
            let view = d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),
                None,
            )?;
            Ok(Scanout { external: true, id, image, view, memory })
        }
    }

    fn import_buffer(&self, fd: OwnedFd) -> Result<Buffer> {
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        if size <= 0 {
            bail!("encoder buffer has no size");
        }
        let d = &self.device;
        unsafe {
            let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(DMA_BUF);
            let buffer = d.create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(vk::BufferUsageFlags::STORAGE_BUFFER).push_next(&mut external), None)?;
            let reqs = d.get_buffer_memory_requirements(buffer);
            let memory = self.import_memory(fd, reqs, None, Some(buffer))?;
            d.bind_buffer_memory(buffer, memory, 0)?;
            Ok(Buffer { buffer, memory })
        }
    }

    fn import_memory(&self, fd: OwnedFd, reqs: vk::MemoryRequirements, image: Option<vk::Image>, buffer: Option<vk::Buffer>) -> Result<vk::DeviceMemory> {
        let mut props = vk::MemoryFdPropertiesKHR::default();
        unsafe { self.memory_fd.get_memory_fd_properties(DMA_BUF, fd.as_raw_fd(), &mut props) }?;
        let bits = reqs.memory_type_bits & props.memory_type_bits;
        if bits == 0 {
            bail!("no memory type fits the shared buffer");
        }
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) }.max(reqs.size as i64) as u64;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default();
        if let Some(i) = image {
            dedicated = dedicated.image(i);
        }
        if let Some(b) = buffer {
            dedicated = dedicated.buffer(b);
        }
        // Vulkan owns the fd once the import succeeds.
        let raw = fd.into_raw_fd();
        let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(DMA_BUF).fd(raw);
        let info = vk::MemoryAllocateInfo::default().allocation_size(size).memory_type_index(bits.trailing_zeros()).push_next(&mut import).push_next(&mut dedicated);
        match unsafe { self.device.allocate_memory(&info, None) } {
            Ok(m) => Ok(m),
            Err(e) => {
                unsafe { libc::close(raw) };
                Err(e).context("importing a shared buffer")
            }
        }
    }

    fn upload(&self, data: &[u8]) -> Result<Buffer> {
        let d = &self.device;
        unsafe {
            let buffer = d.create_buffer(&vk::BufferCreateInfo::default().size(data.len() as u64).usage(vk::BufferUsageFlags::STORAGE_BUFFER), None)?;
            let reqs = d.get_buffer_memory_requirements(buffer);
            let types = self.instance.get_physical_device_memory_properties(self.phys);
            let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let index = (0..types.memory_type_count)
                .find(|&i| reqs.memory_type_bits & (1 << i) != 0 && types.memory_types[i as usize].property_flags.contains(wanted))
                .context("no host-visible memory")?;
            let memory = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(index), None)?;
            d.bind_buffer_memory(buffer, memory, 0)?;
            let ptr = d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            d.unmap_memory(memory);
            Ok(Buffer { buffer, memory })
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let d = &self.device;
        unsafe {
            let _ = d.device_wait_idle();
            for s in self.scanouts.drain(..) {
                d.destroy_image_view(s.view, None);
                d.destroy_image(s.image, None);
                d.free_memory(s.memory, None);
            }
            for b in self.targets.drain(..).flat_map(|t| [t.luma, t.chroma]).chain(std::iter::once(std::mem::replace(&mut self.grid, Buffer { buffer: vk::Buffer::null(), memory: vk::DeviceMemory::null() }))) {
                d.destroy_buffer(b.buffer, None);
                d.free_memory(b.memory, None);
            }
            d.destroy_sampler(self.sampler, None);
            d.destroy_descriptor_pool(self.descriptors, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.pool, None);
            d.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn binding(n: u32, ty: vk::DescriptorType) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default().binding(n).descriptor_type(ty).descriptor_count(1).stage_flags(vk::ShaderStageFlags::COMPUTE)
}

fn whole(buffer: vk::Buffer) -> vk::DescriptorBufferInfo {
    vk::DescriptorBufferInfo::default().buffer(buffer).range(vk::WHOLE_SIZE)
}

fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

/// DRM fourcc to the Vulkan format with the same bytes in memory.
fn vk_format(fourcc: u32) -> Option<vk::Format> {
    let code = |s: &[u8; 4]| u32::from_le_bytes(*s);
    Some(match fourcc {
        f if f == code(b"XR24") || f == code(b"AR24") => vk::Format::B8G8R8A8_UNORM,
        f if f == code(b"XB24") || f == code(b"AB24") => vk::Format::R8G8B8A8_UNORM,
        f if f == code(b"XR30") || f == code(b"AR30") => vk::Format::A2R10G10B10_UNORM_PACK32,
        f if f == code(b"XB30") || f == code(b"AB30") => vk::Format::A2B10G10R10_UNORM_PACK32,
        f if f == code(b"AB4H") || f == code(b"XB4H") => vk::Format::R16G16B16A16_SFLOAT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lens;
    use crate::openvr::Frustum;

    /// Runs the real shader on whatever Vulkan driver is present (Mesa's
    /// software one is enough): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn converts_a_frame_to_nv12() {
        let (w, h) = (64u32, 32u32);
        let f = Frustum { left: -1.0, right: 1.0, top: -1.0, bottom: 1.0 };
        let grid = lens::raw(w, h, [f, f]);
        let mut gpu = Gpu::with_priority(&grid, w, h, false).unwrap();
        // Left half of the "panel" pure red, right half pure green.
        let (sw, sh) = (128u32, 64u32);
        let mut rgba = Vec::new();
        for _ in 0..sh {
            for x in 0..sw {
                rgba.extend_from_slice(if x < sw / 2 { &[255, 0, 0, 255] } else { &[0, 255, 0, 255] });
            }
        }
        gpu.add_test_scanout(1, sw, sh, &rgba).unwrap();
        let t = gpu.add_host_target(w, h).unwrap();
        gpu.convert(1, t).unwrap();
        assert!(gpu.wait(std::time::Duration::from_secs(10)).unwrap().is_some(), "GPU didn't finish");
        let luma = gpu.read_luma(t, (w * h) as usize).unwrap();
        let row = &luma[(h / 2 * w) as usize..((h / 2 + 1) * w) as usize];
        // BT.709 limited range: red Y = 63, green Y = 173.
        assert!((row[4] as i32 - 63).abs() <= 2, "left {:?}", &row[..8]);
        assert!((row[(w - 5) as usize] as i32 - 173).abs() <= 2, "right {:?}", &row[(w - 8) as usize..]);
    }
}
