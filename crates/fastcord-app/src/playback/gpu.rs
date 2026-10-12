//! iced's exact wgpu version; one reusable plane pair for the single watch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use fastcord_video::frame::FrameLease;
use iced::widget::shader;
use iced::{Rectangle, mouse};
use iced_renderer::wgpu::wgpu;

pub(super) struct VideoProgram {
    pub(super) frame: FrameLease,
    pub(super) active: Arc<AtomicBool>,
}

impl<Message> shader::Program<Message> for VideoProgram {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> VideoPrimitive {
        VideoPrimitive {
            frame: self.frame.clone(),
            active: self.active.clone(),
        }
    }
}

#[derive(Debug)]
pub(super) struct VideoPrimitive {
    frame: FrameLease,
    active: Arc<AtomicBool>,
}

struct Textures {
    size: (u32, u32),
    y: wgpu::Texture,
    uv: wgpu::Texture,
    group: wgpu::BindGroup,
    serial: Option<u64>,
    active: Weak<AtomicBool>,
}

pub(super) struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    colors: wgpu::Buffer,
    textures: Option<Textures>,
    used: bool,
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fastcord video planes"),
            entries: &[
                plane_binding(0),
                plane_binding(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(48),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fastcord video layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fastcord NV12 conversion"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fastcord NV12 video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(if format.is_srgb() {
                    "fragment_srgb"
                } else {
                    "fragment_rgb"
                }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fastcord video sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let colors = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fastcord video color matrix"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            colors,
            textures: None,
            used: false,
        }
    }

    fn trim(&mut self) {
        if !self.used
            || self.textures.as_ref().is_some_and(|textures| {
                textures
                    .active
                    .upgrade()
                    .is_none_or(|active| !active.load(Ordering::Acquire))
            })
        {
            self.textures = None;
        }
        self.used = false;
    }
}

fn plane_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn plane(
    device: &wgpu::Device,
    label: &'static str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(image) = self.frame.nv12() else {
            return;
        };
        pipeline.used = true;
        let size = (image.width(), image.height());
        if pipeline
            .textures
            .as_ref()
            .is_none_or(|textures| textures.size != size)
        {
            // Release the previous resolution before allocating its replacement.
            pipeline.textures = None;
            let y = plane(
                device,
                "fastcord video Y",
                size.0,
                size.1,
                wgpu::TextureFormat::R8Unorm,
            );
            let uv = plane(
                device,
                "fastcord video UV",
                size.0.div_ceil(2),
                size.1.div_ceil(2),
                wgpu::TextureFormat::Rg8Unorm,
            );
            let y_view = y.create_view(&Default::default());
            let uv_view = uv.create_view(&Default::default());
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fastcord video planes"),
                layout: &pipeline.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: pipeline.colors.as_entire_binding(),
                    },
                ],
            });
            pipeline.textures = Some(Textures {
                size,
                y,
                uv,
                group,
                serial: None,
                active: Arc::downgrade(&self.active),
            });
        }
        let textures = pipeline
            .textures
            .as_mut()
            .expect("plane pair was allocated");
        let same_session = textures.active.ptr_eq(&Arc::downgrade(&self.active));
        if textures.serial == Some(self.frame.serial()) && same_session {
            return;
        }
        // The lease is tightly packed once on the worker. write_texture handles
        // row staging; unlike encoder buffer copies it permits unaligned strides.
        let bytes = self.frame.pixels();
        let luma_bytes = image.width() as usize * image.height() as usize;
        upload(
            queue,
            &textures.y,
            &bytes[..luma_bytes],
            image.luma_stride() as u32,
            size.0,
            size.1,
        );
        upload(
            queue,
            &textures.uv,
            &bytes[luma_bytes..],
            image.chroma_stride() as u32,
            size.0.div_ceil(2),
            size.1.div_ceil(2),
        );
        queue.write_buffer(
            &pipeline.colors,
            0,
            bytemuck::cast_slice(&self.frame.color().transform_rows()),
        );
        textures.serial = Some(self.frame.serial());
        textures.active = Arc::downgrade(&self.active);
    }

    fn draw(&self, pipeline: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.active.load(Ordering::Acquire)
            && let Some(textures) = &pipeline.textures
        {
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &textures.group, &[]);
            pass.draw(0..6, 0..1);
        }
        true
    }
}

fn upload(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: &[u8],
    stride: u32,
    width: u32,
    height: u32,
) {
    queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}
