//! Tab bar rendering at the top of the terminal window.
//!
//! Displays tabs with titles, close indicators, and shortcut hints.

/// Per-instance data for tab bar background quads.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BarInstance {
    position: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

/// Uniform data for the shader.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport_size: [f32; 2],
}

/// Tab bar info for rendering.
pub struct TabBarState {
    pub tabs: Vec<TabInfo>,
    pub active_index: usize,
}

pub struct TabInfo {
    pub title: String,
}

/// Renders the tab bar at the top of the terminal window.
pub struct StatusBar {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
}

impl StatusBar {
    /// Create a new status bar renderer.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tabbar shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/cell.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tabbar uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tabbar bind group layout"),
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
            label: Some("tabbar bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tabbar pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<BarInstance>() as wgpu::BufferAddress,
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
            label: Some("tabbar pipeline"),
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
            label: Some("tabbar instances"),
            size: (std::mem::size_of::<BarInstance>() * 64) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { pipeline, instance_buffer, instance_count: 0, uniform_buffer, uniform_bind_group }
    }

    /// Update the tab bar instances for the current state.
    pub fn update(
        &mut self,
        queue: &wgpu::Queue,
        device: &wgpu::Device,
        viewport_size: (u32, u32),
        bar_height: f32,
        state: &TabBarState,
    ) {
        let vw = viewport_size.0 as f32;
        let vh = viewport_size.1 as f32;

        let uniforms = Uniforms { viewport_size: [vw, vh] };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let mut instances = Vec::new();

        // Full background bar at the top
        instances.push(BarInstance {
            position: [0.0, 0.0],
            size: [vw, bar_height],
            color: [0.18, 0.18, 0.18, 1.0], // Neutral dark background #2d2d2d
        });

        // Bottom separator line (1px)
        instances.push(BarInstance {
            position: [0.0, bar_height - 1.0],
            size: [vw, 1.0],
            color: [0.24, 0.24, 0.24, 1.0], // Subtle border #3d3d3d
        });

        // Tab buttons
        let tab_padding = 2.0;
        let tab_height = bar_height - 6.0;
        let tab_y = 3.0;
        let min_tab_width = 140.0;
        let max_tab_width = 240.0;

        let num_tabs = state.tabs.len().max(1) as f32;
        let available_width = vw - 200.0; // Reserve space for window controls
        let tab_width =
            (available_width / num_tabs - tab_padding).clamp(min_tab_width, max_tab_width);

        for (i, _tab) in state.tabs.iter().enumerate() {
            let is_active = i == state.active_index;
            let x = tab_padding + i as f32 * (tab_width + tab_padding);

            if x + tab_width > available_width {
                break;
            }

            let color = if is_active {
                [0.12, 0.12, 0.12, 1.0] // Active tab: darker #1e1e1e
            } else {
                [0.18, 0.18, 0.18, 1.0] // Inactive tab: same as bar bg #2d2d2d
            };

            instances.push(BarInstance {
                position: [x, tab_y],
                size: [tab_width, tab_height],
                color,
            });

            // Active tab accent line at the bottom of the tab
            if is_active {
                instances.push(BarInstance {
                    position: [x, tab_y + tab_height - 2.0],
                    size: [tab_width, 2.0],
                    color: [0.35, 0.45, 0.98, 1.0], // Blue accent
                });
            }
        }

        self.instance_count = instances.len() as u32;

        let data = bytemuck::cast_slice(&instances);
        let needed = data.len() as u64;
        if needed > self.instance_buffer.size() {
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tabbar instances"),
                size: needed,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.instance_buffer, 0, data);
    }

    /// Render the tab bar backgrounds.
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
