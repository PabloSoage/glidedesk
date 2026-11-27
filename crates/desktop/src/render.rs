//! Presents decoded pictures: plane textures converted to RGB in the fragment shader
//! (full-range BT.709, as the server encodes) and drawn over the window. 4:2:0 comes as two
//! planes (Y, interleaved UV at half size), 4:4:4 as three full-size ones (Y, U, V), each
//! with its own pipeline. The swap chain does not wait for vblank when the platform allows
//! it (lowest latency).

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

use crate::video::{Chroma, Picture};

/// The vertex stage both pipelines share, and the colour conversion.
const VERTEX: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the viewport.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

// Full-range BT.709.
fn rgb(y: f32, u: f32, v: f32) -> vec4<f32> {
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

const NV12: &str = r#"
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tuv: texture_2d<f32>;
@group(0) @binding(2) var s: sampler;
// Picture size over the texture size (the planes are as wide as their stride).
@group(0) @binding(3) var<uniform> scale: vec4<f32>;

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    let y = textureSample(ty, s, i.uv * scale.xy).r;
    let c = textureSample(tuv, s, i.uv * scale.zw).rg - vec2<f32>(0.5);
    return rgb(y, c.x, c.y);
}
"#;

const YUV444: &str = r#"
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var s: sampler;
// Picture size over the texture size (the planes are as wide as their stride).
@group(0) @binding(4) var<uniform> scale: vec4<f32>;

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    let y = textureSample(ty, s, i.uv * scale.xy).r;
    let u = textureSample(tu, s, i.uv * scale.zw).r - 0.5;
    let v = textureSample(tv, s, i.uv * scale.zw).r - 0.5;
    return rgb(y, u, v);
}
"#;

/// The picture's textures: Y and UV (4:2:0), or Y, U and V (4:4:4).
struct Planes {
    textures: Vec<wgpu::Texture>,
    bind: wgpu::BindGroup,
    /// Drawn with the 4:4:4 pipeline.
    yuv444: bool,
    /// (4:4:4, Y stride, chroma stride, width, height) these were made for.
    key: (bool, u32, u32, u32, u32),
}

/// A pipeline for one layout of planes.
struct Stage {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    nv12: Stage,
    yuv444: Stage,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    planes: Option<Planes>,
}

impl Renderer {
    /// `vendor`: draw on the first GPU of this PCI vendor (`--gpu`) that can show the window.
    pub fn new(
        instance: &wgpu::Instance,
        window: Arc<Window>,
        vendor: Option<u32>,
    ) -> Result<Self> {
        let size = window.inner_size();
        let surface = instance.create_surface(window)?;
        let chosen = vendor.and_then(|v| {
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
                .into_iter()
                .find(|a| a.get_info().vendor == v && a.is_surface_supported(&surface))
        });
        let adapter = match chosen {
            Some(a) => a,
            None => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            }))
            .context("no graphics adapter")?,
        };
        tracing::info!(adapter = ?adapter.get_info().name, backend = ?adapter.get_info().backend, "Graphics");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
        let caps = surface.get_capabilities(&adapter);
        // The YUV values are already gamma-encoded: a non-sRGB target keeps them as they are.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = [wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        tracing::info!(?format, ?present_mode, "Swap chain");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let nv12 = stage(&device, format, "nv12", NV12, 2);
        let yuv444 = stage(&device, format, "yuv444", YUV444, 3);
        // Linear filtering: while the window and the desktop differ in size (during a
        // resize) the picture is scaled; at 1:1 it samples texel centres exactly.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scale"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            nv12,
            yuv444,
            sampler,
            uniform,
            planes: None,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    #[allow(dead_code)]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Renders a frame using a custom drawing closure onto the swapchain.
    pub fn render_frame<F>(&mut self, draw_fn: F) -> bool
    where
        F: FnOnce(&wgpu::Device, &wgpu::Queue, &wgpu::TextureView),
    {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        draw_fn(&self.device, &self.queue, &view);
        self.queue.present(frame);
        true
    }

    fn plane(&self, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: w,
                height: h,
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

    fn upload(&mut self, p: &Picture) {
        let ch = p.height.div_ceil(2);
        // (data, bytes per row, texels per row, rows, format) of each plane.
        let planes: Vec<(&[u8], u32, u32, u32, wgpu::TextureFormat)> = match &p.chroma {
            Chroma::Nv12 { uv, stride } => vec![
                (p.y.as_slice(), p.y_stride, p.y_stride, p.height, wgpu::TextureFormat::R8Unorm),
                (uv.as_slice(), *stride, stride / 2, ch, wgpu::TextureFormat::Rg8Unorm),
            ],
            Chroma::Planar { u, v, stride } => vec![
                (p.y.as_slice(), p.y_stride, p.y_stride, p.height, wgpu::TextureFormat::R8Unorm),
                (u.as_slice(), *stride, *stride, p.height, wgpu::TextureFormat::R8Unorm),
                (v.as_slice(), *stride, *stride, p.height, wgpu::TextureFormat::R8Unorm),
            ],
        };
        let yuv444 = matches!(p.chroma, Chroma::Planar { .. });
        let chroma_stride = planes[1].1;
        let key = (yuv444, p.y_stride, chroma_stride, p.width, p.height);
        if self.planes.as_ref().is_none_or(|pl| pl.key != key) {
            let textures: Vec<wgpu::Texture> = planes
                .iter()
                .map(|&(_, _, texels, rows, format)| self.plane(texels, rows, format))
                .collect();
            let views: Vec<wgpu::TextureView> = textures
                .iter()
                .map(|t| t.create_view(&Default::default()))
                .collect();
            let mut entries: Vec<wgpu::BindGroupEntry> = views
                .iter()
                .enumerate()
                .map(|(i, view)| wgpu::BindGroupEntry {
                    binding: i as u32,
                    resource: wgpu::BindingResource::TextureView(view),
                })
                .collect();
            let n = views.len() as u32;
            entries.push(wgpu::BindGroupEntry {
                binding: n,
                resource: wgpu::BindingResource::Sampler(&self.sampler),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: n + 1,
                resource: self.uniform.as_entire_binding(),
            });
            let stage = if yuv444 { &self.yuv444 } else { &self.nv12 };
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("planes"),
                layout: &stage.layout,
                entries: &entries,
            });
            // The chroma texture holds `width / 2` (4:2:0, two bytes a texel) or `width`
            // (4:4:4) picture columns: either way width over its bytes per row.
            let scale = [
                p.width as f32 / p.y_stride as f32,
                1.0,
                p.width as f32 / chroma_stride as f32,
                1.0,
            ];
            let bytes: Vec<u8> = scale.iter().flat_map(|f| f.to_le_bytes()).collect();
            self.queue.write_buffer(&self.uniform, 0, &bytes);
            self.planes = Some(Planes {
                textures,
                bind,
                yuv444,
                key,
            });
        }
        let pl = self.planes.as_ref().expect("created above");
        for (tex, &(data, stride, texels, rows, _)) in pl.textures.iter().zip(&planes) {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(rows),
                },
                wgpu::Extent3d {
                    width: texels,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Draws `picture` (or the last one again); false if the surface was not available.
    pub fn draw(&mut self, picture: Option<&Picture>) -> bool {
        if let Some(p) = picture {
            self.upload(p);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(pl) = &self.planes {
                let stage = if pl.yuv444 { &self.yuv444 } else { &self.nv12 };
                pass.set_pipeline(&stage.pipeline);
                pass.set_bind_group(0, &pl.bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        true
    }
}

/// The bind group layout (`textures` planes, a sampler, the scale uniform) and the pipeline
/// of the fragment shader `fragment`.
fn stage(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    label: &str,
    fragment: &str,
    textures: u32,
) -> Stage {
    let source = format!("{VERTEX}{fragment}");
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let mut entries: Vec<wgpu::BindGroupLayoutEntry> = (0..textures)
        .map(|binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        })
        .collect();
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: textures,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    });
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: textures + 1,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&layout)],
        ..Default::default()
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(format.into())],
        }),
        multiview_mask: None,
        cache: None,
    });
    Stage { pipeline, layout }
}
