//! wgpu rendering context and surface management.
//!
//! Initializes the GPU device, creates the rendering surface, and manages
//! the render pipeline state. This is the entry point for all GPU operations.

// TODO: Phase 3 — implement RenderContext
//
// pub struct RenderContext {
//     device: wgpu::Device,
//     queue: wgpu::Queue,
//     surface: wgpu::Surface,
//     config: wgpu::SurfaceConfiguration,
//     pipeline: wgpu::RenderPipeline,
// }
//
// impl RenderContext {
//     pub async fn new(window: &impl HasRawWindowHandle) -> Result<Self> { ... }
//     pub fn resize(&mut self, size: Size) { ... }
//     pub fn render(&mut self) -> Result<()> { ... }
// }
