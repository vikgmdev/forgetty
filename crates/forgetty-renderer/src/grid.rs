//! Terminal grid background rendering.
//!
//! Renders cell background colors as solid-colored quads using instanced
//! rendering with a simple WGSL shader.

use crate::atlas::CellSize;
use crate::color::ColorScheme;
use forgetty_vt::Screen;

/// Per-instance data sent to the GPU for each background quad.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BackgroundInstance {
    /// Top-left corner position in pixels.
    position: [f32; 2],
    /// Cell width and height in pixels.
    size: [f32; 2],
    /// RGBA color (0.0 - 1.0).
    color: [f32; 4],
}

/// Uniform data for the cell shader.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport_size: [f32; 2],
}

/// Renders cell background colors as instanced quads.
pub struct BackgroundRenderer {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
}

impl BackgroundRenderer {
    /// Create a new background renderer.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/cell.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bg uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bg bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bg pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<BackgroundInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // size
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Start with an empty instance buffer (will be resized in update)
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bg instances"),
            size: 64, // minimum
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { pipeline, instance_buffer, instance_count: 0, uniform_buffer, uniform_bind_group }
    }

    /// Update the instance buffer with current screen background colors.
    pub fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &Screen,
        cell_size: &CellSize,
        scroll_offset: usize,
        viewport_size: (u32, u32),
        color_scheme: &ColorScheme,
    ) {
        // Update uniform buffer with viewport size
        let uniforms = Uniforms { viewport_size: [viewport_size.0 as f32, viewport_size.1 as f32] };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let default_bg = color_scheme.background;

        // Calculate visible rows
        let visible_rows = (viewport_size.1 as f32 / cell_size.height).ceil() as usize;
        let visible_cols = screen.cols();

        let mut instances = Vec::with_capacity(visible_rows * visible_cols);

        for vis_row in 0..visible_rows {
            let screen_row = vis_row + scroll_offset;
            if screen_row >= screen.rows() {
                break;
            }

            let row = screen.row(screen_row);
            for (col, cell) in row.iter().enumerate().take(visible_cols) {
                let bg = if cell.attrs.inverse {
                    color_scheme.resolve_fg(cell.attrs.fg)
                } else {
                    color_scheme.resolve_bg(cell.attrs.bg)
                };

                // Skip cells with the default background (they'll show through the clear color)
                if bg == default_bg {
                    continue;
                }

                instances.push(BackgroundInstance {
                    position: [col as f32 * cell_size.width, vis_row as f32 * cell_size.height],
                    size: [cell_size.width, cell_size.height],
                    color: [
                        bg[0] as f32 / 255.0,
                        bg[1] as f32 / 255.0,
                        bg[2] as f32 / 255.0,
                        bg[3] as f32 / 255.0,
                    ],
                });
            }
        }

        self.instance_count = instances.len() as u32;

        if instances.is_empty() {
            return;
        }

        let data = bytemuck::cast_slice(&instances);
        let needed = data.len() as u64;

        // Recreate buffer if too small
        if needed > self.instance_buffer.size() {
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bg instances"),
                size: needed,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        queue.write_buffer(&self.instance_buffer, 0, data);
    }

    /// Render background quads into the render pass.
    pub fn render<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..4, 0..self.instance_count);
    }
}
