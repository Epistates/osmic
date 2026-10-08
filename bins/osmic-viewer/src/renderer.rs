//! The device-level renderer: tile meshes drawn with MSAA, then the text
//! overlay, into any color target.
//!
//! It knows nothing about windows or surfaces (see [`crate::gpu`]), which
//! is what lets the tests render into an offscreen texture.
//!
//! Colors: style colors are sRGB values meant to be blended as-is (as
//! MapLibre and browsers do), so the target should be a non-sRGB format.
//! When only an sRGB target exists (`linearize == true`) the shaders convert
//! to linear so the displayed colors are still right instead of washed out.

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use osmic_core::Color;
use osmic_render::{Mesh, MeshVertex};
use tracing::warn;
use wgpu::util::DeviceExt;

/// Draw slots per frame (one per visible tile, with ample headroom).
const MAX_DRAWS: usize = 1024;

/// Per-draw shader inputs (32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct DrawUniform {
    /// Screen position (logical px) of the tile origin.
    pub offset: [f32; 2],
    /// Viewport size in logical px.
    pub viewport: [f32; 2],
    /// Screen pixels per tile pixel.
    pub scale: f32,
    pub tile_zoom: f32,
    pub camera_zoom: f32,
    /// 1.0 when the target is sRGB and colors must be linearised.
    pub linearize: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct OverlayUniform {
    /// `x, y, w, h` of the overlay image in physical pixels.
    rect: [f32; 4],
    viewport: [f32; 2],
    linearize: f32,
    _pad: f32,
}

/// A tile's GPU buffers.
pub struct GpuTile {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// One tile to draw this frame.
pub struct TileDraw<'a> {
    pub tile: &'a GpuTile,
    pub uniform: DrawUniform,
    /// Physical scissor rectangle `[x, y, w, h]`.
    pub scissor: [u32; 4],
}

/// Convert an sRGB component to linear light.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The clear color for a style background on this target.
pub fn clear_color(background: Color, linearize: bool) -> wgpu::Color {
    let f = |c: f32| f64::from(if linearize { srgb_to_linear(c) } else { c });
    wgpu::Color {
        r: f(background.r),
        g: f(background.g),
        b: f(background.b),
        a: f64::from(background.a),
    }
}

struct OverlayGpu {
    texture: wgpu::Texture,
    size: [u32; 2],
    bind_group: wgpu::BindGroup,
    uniform: wgpu::Buffer,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    linearize: bool,
    samples: u32,
    size: [u32; 2],
    msaa: Option<wgpu::TextureView>,
    map_pipeline: wgpu::RenderPipeline,
    draw_buffer: wgpu::Buffer,
    draw_stride: u64,
    draw_bind_group: wgpu::BindGroup,
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_layout: wgpu::BindGroupLayout,
    overlay: OverlayGpu,
    overlay_shift: [i32; 2],
}

impl Renderer {
    /// Create the pipelines for color targets of `format` and `size`
    /// physical pixels, with `samples`x MSAA for the map pass.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        linearize: bool,
        samples: u32,
        size: [u32; 2],
    ) -> Self {
        // --- map pipeline ---
        let draw_stride = u64::from(device.limits().min_uniform_buffer_offset_alignment).max(256);
        let draw_size = NonZeroU64::new(std::mem::size_of::<DrawUniform>() as u64);
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("draw uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: draw_size,
                },
                count: None,
            }],
        });
        let draw_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("draw uniforms"),
            size: draw_stride * MAX_DRAWS as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("draw uniforms"),
            layout: &draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &draw_buffer,
                    offset: 0,
                    size: draw_size,
                }),
            }],
        });
        let map_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("map shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("map.wgsl").into()),
        });
        let map_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("map pipeline layout"),
            bind_group_layouts: &[Some(&draw_layout)],
            immediate_size: 0,
        });
        let vertex_attributes = [
            wgpu::VertexAttribute {
                offset: std::mem::offset_of!(MeshVertex, position) as u64,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: std::mem::offset_of!(MeshVertex, extrude) as u64,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: std::mem::offset_of!(MeshVertex, half_width) as u64,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: std::mem::offset_of!(MeshVertex, color) as u64,
                shader_location: 3,
                format: wgpu::VertexFormat::Unorm8x4,
            },
        ];
        let map_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("map pipeline"),
            layout: Some(&map_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &map_shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<MeshVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &vertex_attributes,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &map_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: samples,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        // --- overlay pipeline ---
        let overlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let overlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
        });
        let overlay_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("overlay pipeline layout"),
                bind_group_layouts: &[Some(&overlay_layout)],
                immediate_size: 0,
            });
        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay pipeline"),
            layout: Some(&overlay_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &overlay_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &overlay_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let overlay = Self::create_overlay(&device, &overlay_layout, size);
        let mut renderer = Self {
            device,
            queue,
            format,
            linearize,
            samples,
            size,
            msaa: None,
            map_pipeline,
            draw_buffer,
            draw_stride,
            draw_bind_group,
            overlay_pipeline,
            overlay_layout,
            overlay,
            overlay_shift: [0, 0],
        };
        renderer.msaa = renderer.create_msaa();
        renderer
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Whether colors are linearised in the shaders (sRGB target).
    pub fn linearize(&self) -> bool {
        self.linearize
    }

    /// Change the target size. The MSAA target and the label overlay are
    /// recreated at the new size, so a larger window never samples or
    /// copies out of bounds.
    pub fn resize(&mut self, size: [u32; 2]) {
        if size == self.size || size[0] == 0 || size[1] == 0 {
            return;
        }
        self.size = size;
        self.msaa = self.create_msaa();
        self.overlay = Self::create_overlay(&self.device, &self.overlay_layout, size);
        self.overlay_shift = [0, 0];
    }

    fn create_msaa(&self) -> Option<wgpu::TextureView> {
        (self.samples > 1).then(|| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("msaa target"),
                    size: wgpu::Extent3d {
                        width: self.size[0],
                        height: self.size[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: self.samples,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        })
    }

    fn create_overlay(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        size: [u32; 2],
    ) -> OverlayGpu {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("label overlay"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Raw (non-sRGB) bytes: the overlay holds premultiplied sRGB
            // values and is blended as-is, like the map.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("overlay uniform"),
            contents: bytemuck::bytes_of(&OverlayUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("label overlay"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        OverlayGpu {
            texture,
            size,
            bind_group,
            uniform,
        }
    }

    /// Upload a tile's mesh. `None` for an empty mesh.
    pub fn upload_tile(&self, mesh: &Mesh) -> Option<GpuTile> {
        if mesh.vertices.is_empty() || mesh.indices.is_empty() {
            return None;
        }
        let max = self.device.limits().max_buffer_size;
        let vertex_bytes = std::mem::size_of_val(mesh.vertices.as_slice()) as u64;
        let index_bytes = std::mem::size_of_val(mesh.indices.as_slice()) as u64;
        if vertex_bytes > max || index_bytes > max {
            warn!(
                vertex_bytes,
                index_bytes, "tile mesh exceeds the GPU buffer limit; skipped"
            );
            return None;
        }
        let vertices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tile vertices"),
                contents: bytemuck::cast_slice(&mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let indices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tile indices"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        Some(GpuTile {
            vertices,
            indices,
            index_count: mesh.indices.len() as u32,
        })
    }

    /// Replace the overlay image. `rgba` is premultiplied, `size` physical
    /// pixels; a size that differs from the current overlay recreates it.
    pub fn upload_overlay(&mut self, rgba: &[u8], size: [u32; 2]) {
        if size[0] == 0 || size[1] == 0 || rgba.len() != size[0] as usize * size[1] as usize * 4 {
            return;
        }
        if size != self.overlay.size {
            self.overlay = Self::create_overlay(&self.device, &self.overlay_layout, size);
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.overlay.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size[0] * 4),
                rows_per_image: Some(size[1]),
            },
            wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
        );
        self.overlay_shift = [0, 0];
    }

    /// Move the overlay image by whole physical pixels (it follows the map
    /// between re-layouts).
    pub fn set_overlay_shift(&mut self, shift: [i32; 2]) {
        self.overlay_shift = shift;
    }

    /// Draw the tiles, then the overlay, into `view` (a target of the
    /// renderer's format and size) and submit.
    pub fn draw(&mut self, view: &wgpu::TextureView, clear: Color, draws: &[TileDraw<'_>]) {
        let draws = &draws[..draws.len().min(MAX_DRAWS)];
        for (i, d) in draws.iter().enumerate() {
            self.queue.write_buffer(
                &self.draw_buffer,
                i as u64 * self.draw_stride,
                bytemuck::bytes_of(&d.uniform),
            );
        }
        let [w, h] = self.size;
        self.queue.write_buffer(
            &self.overlay.uniform,
            0,
            bytemuck::bytes_of(&OverlayUniform {
                rect: [
                    self.overlay_shift[0] as f32,
                    self.overlay_shift[1] as f32,
                    self.overlay.size[0] as f32,
                    self.overlay.size[1] as f32,
                ],
                viewport: [w as f32, h as f32],
                linearize: if self.linearize { 1.0 } else { 0.0 },
                _pad: 0.0,
            }),
        );

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let (target, resolve) = match &self.msaa {
                Some(msaa) => (msaa, Some(view)),
                None => (view, None),
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("map"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(clear, self.linearize)),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.map_pipeline);
            for (i, d) in draws.iter().enumerate() {
                let [x, y, sw, sh] = d.scissor;
                if sw == 0 || sh == 0 || x >= w || y >= h {
                    continue;
                }
                pass.set_scissor_rect(x, y, sw.min(w - x), sh.min(h - y));
                pass.set_bind_group(
                    0,
                    &self.draw_bind_group,
                    &[(i as u64 * self.draw_stride) as u32],
                );
                pass.set_vertex_buffer(0, d.tile.vertices.slice(..));
                pass.set_index_buffer(d.tile.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..d.tile.index_count, 0, 0..1);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.overlay_pipeline);
            pass.set_bind_group(0, &self.overlay.bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }
}

/// The shader uniform for a planned draw.
pub fn tile_draw_uniform(
    transform: osmic_render::TileTransform,
    tile_zoom: u8,
    camera_zoom: f64,
    logical_viewport: [f64; 2],
    linearize: bool,
) -> DrawUniform {
    DrawUniform {
        offset: [transform.offset[0] as f32, transform.offset[1] as f32],
        viewport: [logical_viewport[0] as f32, logical_viewport[1] as f32],
        scale: transform.scale as f32,
        tile_zoom: f32::from(tile_zoom),
        camera_zoom: camera_zoom as f32,
        linearize: if linearize { 1.0 } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_conversion_matches_the_standard() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_to_linear(0.5) - 0.214_04).abs() < 1e-4);
        assert!((srgb_to_linear(0.02) - 0.02 / 12.92).abs() < 1e-9);
    }

    #[test]
    fn clear_color_is_linearised_only_for_srgb_targets() {
        let c = Color::rgb(0.5, 0.5, 0.5);
        assert_eq!(clear_color(c, false).r, 0.5);
        assert!((clear_color(c, true).r - 0.214_04).abs() < 1e-4);
        assert_eq!(
            clear_color(Color::rgba(1.0, 1.0, 1.0, 0.25), true).a,
            0.25,
            "alpha is never converted"
        );
    }

    /// Shaders cannot be compiled without a GPU here, but they can be parsed
    /// and validated against the same rules wgpu applies. (The headless GPU
    /// test in `gpu_tests` compiles them for real when an adapter exists.)
    #[test]
    fn shaders_are_valid_wgsl() {
        for (name, source) in [
            ("map.wgsl", include_str!("map.wgsl")),
            ("overlay.wgsl", include_str!("overlay.wgsl")),
        ] {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::empty(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
    }

    #[test]
    fn mesh_vertex_matches_the_shader_attribute_layout() {
        assert_eq!(std::mem::offset_of!(MeshVertex, position), 0);
        assert_eq!(std::mem::offset_of!(MeshVertex, extrude), 8);
        assert_eq!(std::mem::offset_of!(MeshVertex, half_width), 16);
        assert_eq!(std::mem::offset_of!(MeshVertex, color), 24);
        assert_eq!(std::mem::size_of::<MeshVertex>(), 28);
    }

    #[test]
    fn shader_uniforms_have_the_expected_layout() {
        assert_eq!(std::mem::size_of::<DrawUniform>(), 32);
        assert_eq!(std::mem::size_of::<OverlayUniform>(), 32);
    }

    #[test]
    fn draw_uniform_carries_the_transform() {
        let u = tile_draw_uniform(
            osmic_render::TileTransform {
                offset: [10.0, 20.0],
                scale: 2.0,
            },
            12,
            12.5,
            [800.0, 600.0],
            true,
        );
        assert_eq!(
            (u.offset, u.scale, u.tile_zoom, u.camera_zoom, u.linearize),
            ([10.0, 20.0], 2.0, 12.0, 12.5, 1.0)
        );
        assert_eq!(u.viewport, [800.0, 600.0]);
    }

    // --- Headless GPU tests -------------------------------------------
    //
    // These render into an offscreen texture on whatever adapter the machine
    // has and are skipped (with a note on stderr) when there is none, such
    // as on a CI runner without a GPU.

    use osmic_render::{
        RenderFeature, RenderLayer, SceneGraph, TessellationOptions, TileTransform,
        tessellate_scene,
    };

    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    fn offscreen(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        size: [u32; 2],
    ) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// Read back an RGBA8 texture (width must make rows 256-byte aligned).
    fn read_back(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        size: [u32; 2],
    ) -> Vec<u8> {
        assert_eq!((size[0] * 4) % 256, 0, "test sizes keep rows aligned");
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(size[0] * size[1] * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size[0] * 4),
                    rows_per_image: Some(size[1]),
                },
            },
            wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map callback").expect("map");
        let data = buffer
            .slice(..)
            .get_mapped_range()
            .expect("mapped")
            .to_vec();
        buffer.unmap();
        data
    }

    struct Harness {
        renderer: Renderer,
        texture: wgpu::Texture,
        size: [u32; 2],
        scope: Option<wgpu::ErrorScopeGuard>,
    }

    impl Harness {
        fn new(
            format: wgpu::TextureFormat,
            linearize: bool,
            samples: u32,
            size: [u32; 2],
        ) -> Option<Self> {
            let (device, queue) = headless_device().or_else(|| {
                eprintln!("no GPU adapter available; skipping headless GPU test");
                None
            })?;
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let texture = offscreen(&device, format, size);
            Some(Self {
                renderer: Renderer::new(device, queue, format, linearize, samples, size),
                texture,
                size,
                scope: Some(scope),
            })
        }

        fn render(&mut self, clear: Color, draws: &[TileDraw<'_>]) -> Vec<u8> {
            let view = self
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.renderer.draw(&view, clear, draws);
            read_back(
                self.renderer.device(),
                self.renderer.queue(),
                &self.texture,
                self.size,
            )
        }

        fn assert_no_validation_errors(&mut self) {
            let scope = self.scope.take().expect("checked once");
            let error = pollster::block_on(scope.pop());
            assert!(error.is_none(), "wgpu validation error: {error:?}");
        }
    }

    fn pixel(data: &[u8], size: [u32; 2], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size[0] + x) * 4) as usize;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    fn mesh_of(features: Vec<RenderFeature>) -> Mesh {
        let mut layer = RenderLayer::new(0);
        layer.features = features;
        let mut scene = SceneGraph::new(Color::WHITE);
        scene.add_layer(layer);
        tessellate_scene(&scene, &TessellationOptions::default())
    }

    fn hline(y: f32, width: f32, next: f32) -> RenderFeature {
        RenderFeature::Stroke {
            coords: vec![[0.0, y], [512.0, y]],
            color: Color::rgb(1.0, 0.0, 0.0),
            width,
            width_next_zoom: next,
            cap: osmic_render::scene::LineCap::Butt,
            join: osmic_render::scene::LineJoin::Miter,
            dash: vec![],
        }
    }

    /// Thickness of a red line on white along column `x`, from coverage.
    fn thickness(data: &[u8], size: [u32; 2], x: u32) -> f32 {
        (0..size[1])
            .map(|y| 1.0 - f32::from(pixel(data, size, x, y)[1]) / 255.0)
            .sum()
    }

    fn draw_at(
        tile: &GpuTile,
        scale: f64,
        tile_zoom: u8,
        camera_zoom: f64,
        size: [u32; 2],
    ) -> TileDraw<'_> {
        TileDraw {
            tile,
            uniform: tile_draw_uniform(
                TileTransform {
                    offset: [0.0, 0.0],
                    scale,
                },
                tile_zoom,
                camera_zoom,
                [f64::from(size[0]), f64::from(size[1])],
                false,
            ),
            scissor: [0, 0, size[0], size[1]],
        }
    }

    #[test]
    fn gpu_strokes_keep_their_screen_width_at_any_zoom() {
        let size = [64, 64];
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8Unorm, false, 4, size) else {
            return;
        };
        let mesh = mesh_of(vec![hline(256.0, 8.0, 8.0)]);
        let tile = h.renderer.upload_tile(&mesh).expect("mesh uploads");
        // The same tile shown at three scales: the line is 8 px thick each time.
        for scale in [0.125, 0.0625, 0.1] {
            let data = h.render(Color::WHITE, &[draw_at(&tile, scale, 0, 0.0, size)]);
            let t = thickness(&data, size, 8);
            assert!((t - 8.0).abs() < 0.4, "scale {scale}: {t}");
        }
        h.assert_no_validation_errors();
    }

    #[test]
    fn gpu_widths_interpolate_between_zoom_levels() {
        let size = [64, 64];
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8Unorm, false, 4, size) else {
            return;
        };
        let tile = h
            .renderer
            .upload_tile(&mesh_of(vec![hline(256.0, 4.0, 12.0)]))
            .unwrap();
        let at = |h: &mut Harness, camera_zoom: f64| {
            let data = h.render(Color::WHITE, &[draw_at(&tile, 0.125, 3, camera_zoom, size)]);
            thickness(&data, size, 32)
        };
        let (t0, t_half, t1) = (at(&mut h, 3.0), at(&mut h, 3.5), at(&mut h, 4.0));
        assert!(
            (t0 - 4.0).abs() < 0.4 && (t_half - 8.0).abs() < 0.4 && (t1 - 12.0).abs() < 0.4,
            "{t0} {t_half} {t1}"
        );
        // Beyond the next zoom the width stays put instead of growing.
        assert!((at(&mut h, 6.0) - 12.0).abs() < 0.4);
        h.assert_no_validation_errors();
    }

    #[test]
    fn gpu_fills_are_placed_scaled_and_scissored() {
        let size = [64, 64];
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8Unorm, false, 4, size) else {
            return;
        };
        let fill = RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [512.0, 0.0], [512.0, 512.0], [0.0, 512.0]]],
            color: Color::rgb(0.0, 0.0, 1.0),
        };
        let tile = h.renderer.upload_tile(&mesh_of(vec![fill])).unwrap();
        // 512 tile px at scale 1/16 = 32 screen px, placed at (16, 8).
        let mut draw = draw_at(&tile, 1.0 / 16.0, 0, 0.0, size);
        draw.uniform.offset = [16.0, 8.0];
        let data = h.render(Color::WHITE, &[draw]);
        assert_eq!(pixel(&data, size, 30, 20), [0, 0, 255, 255]);
        assert_eq!(
            pixel(&data, size, 10, 20),
            [255, 255, 255, 255],
            "left of the tile"
        );
        assert_eq!(
            pixel(&data, size, 30, 45),
            [255, 255, 255, 255],
            "below the tile"
        );
        // Scissored to the left half of the tile.
        let mut draw = draw_at(&tile, 1.0 / 16.0, 0, 0.0, size);
        draw.uniform.offset = [16.0, 8.0];
        draw.scissor = [0, 0, 32, 64];
        let data = h.render(Color::WHITE, &[draw]);
        assert_eq!(pixel(&data, size, 20, 20), [0, 0, 255, 255]);
        assert_eq!(
            pixel(&data, size, 40, 20),
            [255, 255, 255, 255],
            "clipped by the scissor"
        );
        h.assert_no_validation_errors();
    }

    #[test]
    fn gpu_overlay_is_drawn_on_top_and_shifts() {
        let size = [64, 64];
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8Unorm, false, 1, size) else {
            return;
        };
        let mut overlay = vec![0u8; 64 * 64 * 4];
        for y in 10..14 {
            for x in 10..14 {
                let i = (y * 64 + x) * 4;
                // Premultiplied: half-transparent red.
                overlay[i..i + 4].copy_from_slice(&[128, 0, 0, 128]);
            }
        }
        h.renderer.upload_overlay(&overlay, size);
        let data = h.render(Color::rgb(0.0, 1.0, 0.0), &[]);
        let p = pixel(&data, size, 11, 11);
        assert!(
            (i32::from(p[0]) - 128).abs() <= 2 && (i32::from(p[1]) - 127).abs() <= 2,
            "premultiplied blend: {p:?}"
        );
        assert_eq!(pixel(&data, size, 5, 5), [0, 255, 0, 255]);
        h.renderer.set_overlay_shift([5, 3]);
        let data = h.render(Color::rgb(0.0, 1.0, 0.0), &[]);
        assert_eq!(pixel(&data, size, 11, 11), [0, 255, 0, 255], "moved away");
        assert!(
            pixel(&data, size, 16, 14)[0] > 100,
            "moved to the shifted position"
        );
        h.assert_no_validation_errors();
    }

    #[test]
    fn gpu_resizing_recreates_the_overlay_and_msaa_targets() {
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8Unorm, false, 4, [64, 64]) else {
            return;
        };
        // Grow the window: everything the renderer owns follows.
        let bigger = [128, 64];
        h.renderer.resize(bigger);
        h.texture = offscreen(h.renderer.device(), wgpu::TextureFormat::Rgba8Unorm, bigger);
        h.size = bigger;
        let mut overlay = vec![0u8; 128 * 64 * 4];
        overlay[(60 * 128 + 120) * 4..(60 * 128 + 120) * 4 + 4]
            .copy_from_slice(&[255, 255, 255, 255]);
        h.renderer.upload_overlay(&overlay, bigger);
        let data = h.render(Color::BLACK, &[]);
        assert_eq!(pixel(&data, bigger, 120, 60), [255, 255, 255, 255]);
        h.assert_no_validation_errors();
    }

    #[test]
    fn gpu_srgb_targets_are_linearised_so_colors_are_not_washed_out() {
        let size = [64, 64];
        let Some(mut h) = Harness::new(wgpu::TextureFormat::Rgba8UnormSrgb, true, 1, size) else {
            return;
        };
        let fill = RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [512.0, 0.0], [512.0, 512.0], [0.0, 512.0]]],
            color: Color::rgb(0.5, 0.5, 0.5),
        };
        let tile = h.renderer.upload_tile(&mesh_of(vec![fill])).unwrap();
        let data = h.render(
            Color::rgb(0.25, 0.25, 0.25),
            &[draw_at(&tile, 0.0625, 0, 0.0, size).with_linearize(true)],
        );
        // The sRGB framebuffer re-encodes the linear values, so the bytes
        // read back are the style's own sRGB values (0.5 -> 128, 0.25 -> 64).
        let p = pixel(&data, size, 10, 10);
        assert!((i32::from(p[0]) - 128).abs() <= 2, "{p:?}");
        let background = pixel(&data, size, 60, 60);
        assert!((i32::from(background[0]) - 64).abs() <= 2, "{background:?}");
        h.assert_no_validation_errors();
    }

    impl TileDraw<'_> {
        fn with_linearize(mut self, on: bool) -> Self {
            self.uniform.linearize = if on { 1.0 } else { 0.0 };
            self
        }
    }
}
