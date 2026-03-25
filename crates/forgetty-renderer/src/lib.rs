//! GPU-accelerated terminal renderer using wgpu.
//!
//! This crate handles all rendering for the Forgetty terminal, including:
//! - Text glyph rasterization and atlas management via cosmic-text/glyphon
//! - Terminal grid cell rendering via custom wgpu shaders
//! - Cursor rendering with multiple styles (block, bar, underline)
//! - Selection highlight overlay
//! - Damage tracking for efficient partial redraws
//! - Inline image rendering (iTerm2/Sixel protocols)

pub mod atlas;
pub mod color;
pub mod context;
pub mod cursor;
pub mod damage;
pub mod grid;
pub mod images;
pub mod selection;

pub use atlas::{CellSize, GlyphAtlas};
pub use color::ColorScheme;
pub use context::RenderContext;
pub use cursor::{CursorRenderer, CursorStyle};
pub use damage::DamageTracker;
pub use grid::BackgroundRenderer;

use forgetty_vt::Terminal;
use std::sync::Arc;

/// Errors that can occur during rendering.
#[derive(Debug, thiserror::Error)]
pub enum RendererError {
    #[error("surface error: {0}")]
    Surface(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("render error: {0}")]
    Render(String),
}

/// The top-level terminal renderer that ties all rendering subsystems together.
pub struct TerminalRenderer {
    context: RenderContext,
    atlas: GlyphAtlas,
    background: BackgroundRenderer,
    cursor_renderer: CursorRenderer,
    color_scheme: ColorScheme,
    damage: DamageTracker,
    scroll_offset: usize,
    cursor_style: CursorStyle,
}

impl TerminalRenderer {
    /// Create a new terminal renderer attached to the given window.
    pub fn new(
        window: Arc<winit::window::Window>,
        font_family: &str,
        font_size: f32,
    ) -> Result<Self, RendererError> {
        let context = RenderContext::new(window)?;
        let format = context.format();
        let device = &context.device;
        let queue = &context.queue;

        let atlas = GlyphAtlas::new(device, queue, format, font_family, font_size);
        let background = BackgroundRenderer::new(device, format);
        let cursor_renderer = CursorRenderer::new(device, format);
        let color_scheme = ColorScheme::default();
        let damage = DamageTracker::new(24); // sensible default

        Ok(Self {
            context,
            atlas,
            background,
            cursor_renderer,
            color_scheme,
            damage,
            scroll_offset: 0,
            cursor_style: CursorStyle::Block,
        })
    }

    /// Render a frame from the current terminal state.
    pub fn render(&mut self, terminal: &Terminal) -> Result<(), RendererError> {
        let screen = terminal.screen();
        let viewport_size = self.context.size;

        // Step 1: Get surface texture
        let output = self
            .context
            .surface
            .get_current_texture()
            .map_err(|e| RendererError::Render(e.to_string()))?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Step 2: Update background instances
        self.background.update(
            &self.context.device,
            &self.context.queue,
            screen,
            &self.atlas.cell_size(),
            self.scroll_offset,
            viewport_size,
            &self.color_scheme,
        );

        // Step 3: Prepare text
        self.atlas
            .prepare(
                &self.context.device,
                &self.context.queue,
                screen,
                self.scroll_offset,
                viewport_size,
                &self.color_scheme,
            )
            .map_err(|e| RendererError::Render(format!("text prepare: {e:?}")))?;

        // Step 4: Build command buffer
        let mut encoder =
            self.context.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("forgetty render encoder"),
            });

        // Clear color from the color scheme background
        let bg = &self.color_scheme.background;
        let clear_color = wgpu::Color {
            r: bg[0] as f64 / 255.0,
            g: bg[1] as f64 / 255.0,
            b: bg[2] as f64 / 255.0,
            a: bg[3] as f64 / 255.0,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("forgetty render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Pass 1: Backgrounds
            self.background.render(&mut pass);

            // Pass 2: Text
            self.atlas
                .render(&mut pass)
                .map_err(|e| RendererError::Render(format!("text render: {e:?}")))?;

            // Pass 3: Cursor
            let (cursor_row, cursor_col) = terminal.cursor();
            self.cursor_renderer.render(
                &mut pass,
                &self.context.queue,
                (cursor_row, cursor_col),
                &self.atlas.cell_size(),
                terminal.cursor_visible(),
                self.cursor_style,
                viewport_size,
                self.color_scheme.cursor,
            );
        }

        // Step 5: Submit and present
        self.context.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        // Step 6: Post-frame maintenance
        self.atlas.trim();
        self.damage.mark_clean(screen);

        Ok(())
    }

    /// Resize the renderer to new pixel dimensions.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.context.resize(width, height);
        let grid = self.grid_size();
        self.damage.resize(grid.0);
    }

    /// Calculate how many rows and columns fit in the current viewport.
    pub fn grid_size(&self) -> (usize, usize) {
        let cell = self.atlas.cell_size();
        let (w, h) = self.context.size;
        let cols = (w as f32 / cell.width).floor() as usize;
        let rows = (h as f32 / cell.height).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// Set the scroll offset (number of rows scrolled back).
    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset;
    }

    /// Update the color scheme.
    pub fn set_color_scheme(&mut self, scheme: ColorScheme) {
        self.color_scheme = scheme;
    }

    /// Set the cursor style.
    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.cursor_style = style;
    }

    /// Get a reference to the render context.
    pub fn context(&self) -> &RenderContext {
        &self.context
    }

    /// Get a mutable reference to the render context.
    pub fn context_mut(&mut self) -> &mut RenderContext {
        &mut self.context
    }
}
