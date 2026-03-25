//! Cursor rendering.
//!
//! Renders the terminal cursor in various styles (block, bar, underline)
//! with support for blinking animation.

use crate::atlas::CellSize;

/// Cursor display style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// Solid block covering the entire cell.
    Block,
    /// Thin vertical bar at the left edge of the cell.
    Bar,
    /// Horizontal line at the bottom of the cell.
    Underline,
}

/// Per-instance data for cursor rendering (same layout as BackgroundInstance).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct CursorInstance {
    position: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

/// Uniform data for the cursor shader (same layout as grid Uniforms).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport_size: [f32; 2],
}

/// Renders the terminal cursor as a colored quad.
pub struct CursorRenderer {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
}

impl CursorRenderer {
    /// Create a new cursor renderer. Reuses the same cell shader as BackgroundRenderer.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cursor shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/cell.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cursor uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cursor bind group layout"),
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
            label: Some("cursor bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cursor pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<CursorInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cursor pipeline"),
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

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cursor instance"),
            size: std::mem::size_of::<CursorInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { pipeline, instance_buffer, uniform_buffer, uniform_bind_group }
    }

    /// Render the cursor at the given position.
    #[allow(clippy::too_many_arguments)]
    pub fn render<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        queue: &wgpu::Queue,
        cursor_pos: (usize, usize),
        cell_size: &CellSize,
        visible: bool,
        style: CursorStyle,
        viewport_size: (u32, u32),
        cursor_color: [u8; 4],
    ) {
        if !visible {
            return;
        }

        let uniforms = Uniforms { viewport_size: [viewport_size.0 as f32, viewport_size.1 as f32] };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let (row, col) = cursor_pos;
        let x = col as f32 * cell_size.width;
        let y = row as f32 * cell_size.height;

        let (w, h, cy) = match style {
            CursorStyle::Block => (cell_size.width, cell_size.height, y),
            CursorStyle::Bar => (2.0, cell_size.height, y),
            CursorStyle::Underline => (cell_size.width, 2.0, y + cell_size.height - 2.0),
        };

        let instance = CursorInstance {
            position: [x, cy],
            size: [w, h],
            color: [
                cursor_color[0] as f32 / 255.0,
                cursor_color[1] as f32 / 255.0,
                cursor_color[2] as f32 / 255.0,
                cursor_color[3] as f32 / 255.0,
            ],
        };

        queue.write_buffer(&self.instance_buffer, 0, bytemuck::bytes_of(&instance));

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..4, 0..1);
    }
}
