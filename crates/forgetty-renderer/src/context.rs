//! wgpu rendering context and surface management.
//!
//! Initializes the GPU device, creates the rendering surface, and manages
//! the render pipeline state. This is the entry point for all GPU operations.

use std::sync::Arc;

/// Manages the wgpu device, queue, surface, and surface configuration.
pub struct RenderContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub size: (u32, u32),
}

impl RenderContext {
    /// Create a new render context from a winit window.
    ///
    /// This initializes the wgpu instance, adapter, device, queue, and surface.
    /// The window must be wrapped in an `Arc` so the surface can hold a 'static reference.
    pub fn new(window: Arc<winit::window::Window>) -> Result<Self, crate::RendererError> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance
            .create_surface(window)
            .map_err(|e| crate::RendererError::Surface(e.to_string()))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| crate::RendererError::Surface("No suitable GPU adapter found".into()))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor { label: Some("forgetty-renderer"), ..Default::default() },
            None,
        ))
        .map_err(|e| crate::RendererError::Device(e.to_string()))?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer non-sRGB format so we can pass colors directly without gamma conversion.
        // If only sRGB is available, we'll need to handle the conversion in shaders.
        let format = caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first())
            .copied()
            .unwrap_or(wgpu::TextureFormat::Bgra8Unorm);

        // Prefer Mailbox (low-latency, no tearing) over Fifo (vsync but more latency)
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &surface_config);

        Ok(Self { device, queue, surface, surface_config, size: (width, height) })
    }

    /// Resize the surface to new dimensions.
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        self.size = (width, height);
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Get the current surface texture format.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.surface_config.format
    }
}
