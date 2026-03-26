//! GPU-accelerated terminal renderer using wgpu.

pub mod atlas;
pub mod color;
pub mod context;
pub mod cursor;
pub mod damage;
pub mod grid;
pub mod images;
pub mod selection;
pub mod statusbar;

pub use atlas::{CellSize, GlyphAtlas};
pub use color::ColorScheme;
pub use context::RenderContext;
pub use cursor::{CursorRenderer, CursorStyle};
pub use damage::DamageTracker;
pub use grid::BackgroundRenderer;
pub use statusbar::{StatusBar, TabBarState, TabInfo};

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

/// Height of the tab bar in pixels.
const TAB_BAR_HEIGHT: f32 = 36.0;

/// The top-level terminal renderer.
pub struct TerminalRenderer {
    context: RenderContext,
    atlas: GlyphAtlas,
    background: BackgroundRenderer,
    cursor_renderer: CursorRenderer,
    tab_bar: StatusBar,
    color_scheme: ColorScheme,
    damage: DamageTracker,
    scroll_offset: usize,
    cursor_style: CursorStyle,
    last_screen_generation: u64,
    tab_state: TabBarState,
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
        let tab_bar = StatusBar::new(device, format);
        let color_scheme = ColorScheme::default();
        let damage = DamageTracker::new(24);

        Ok(Self {
            context,
            atlas,
            background,
            cursor_renderer,
            tab_bar,
            color_scheme,
            damage,
            scroll_offset: 0,
            cursor_style: CursorStyle::Bar,
            last_screen_generation: 0,
            tab_state: TabBarState {
                tabs: vec![TabInfo { title: "shell".to_string() }],
                active_index: 0,
            },
        })
    }

    /// Render a frame from the current terminal state.
    pub fn render(&mut self, terminal: &Terminal) -> Result<(), RendererError> {
        let screen = terminal.screen();
        let viewport_size = self.context.size;
        let current_gen = screen.generation();
        // Always re-prepare until libghostty-vt dirty tracking is confirmed working
        let screen_changed = true;

        // Terminal content is offset below the tab bar
        let terminal_y_offset = TAB_BAR_HEIGHT;

        // Step 1: Get surface texture
        let output = self
            .context
            .surface
            .get_current_texture()
            .map_err(|e| RendererError::Render(e.to_string()))?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        if screen_changed {
            // Step 2: Update background instances (offset by tab bar)
            self.background.update_with_offset(
                &self.context.device,
                &self.context.queue,
                screen,
                &self.atlas.cell_size(),
                self.scroll_offset,
                viewport_size,
                &self.color_scheme,
                terminal_y_offset,
            );

            // Step 3: Prepare terminal text (offset by tab bar)
            self.atlas
                .prepare_with_offset(
                    &self.context.device,
                    &self.context.queue,
                    screen,
                    self.scroll_offset,
                    viewport_size,
                    &self.color_scheme,
                    terminal_y_offset,
                    &self.tab_state,
                    TAB_BAR_HEIGHT,
                )
                .map_err(|e| RendererError::Render(format!("text prepare: {e:?}")))?;

            self.last_screen_generation = current_gen;
        }

        // Step 4: Update tab bar
        self.tab_bar.update(
            &self.context.queue,
            &self.context.device,
            viewport_size,
            TAB_BAR_HEIGHT,
            &self.tab_state,
        );

        // Step 5: Build command buffer
        let mut encoder =
            self.context.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("forgetty render encoder"),
            });

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

            // Pass 1: Tab bar background (at top)
            self.tab_bar.render(&mut pass);

            // Pass 2: Terminal cell backgrounds (below tab bar)
            self.background.render(&mut pass);

            // Pass 3: All text (tab titles + terminal text)
            self.atlas
                .render(&mut pass)
                .map_err(|e| RendererError::Render(format!("text render: {e:?}")))?;

            // Pass 4: Cursor (offset by tab bar)
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
                terminal_y_offset,
            );
        }

        // Step 6: Submit and present
        self.context.queue.submit(std::iter::once(encoder.finish()));
        output.present();

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
        let usable_h = h as f32 - TAB_BAR_HEIGHT;
        let cols = (w as f32 / cell.width).floor() as usize;
        let rows = (usable_h / cell.height).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// Update the tab bar state.
    pub fn set_tab_info(&mut self, tabs: Vec<(String, bool)>) {
        self.tab_state.active_index = tabs.iter().position(|(_, active)| *active).unwrap_or(0);
        self.tab_state.tabs = tabs.into_iter().map(|(title, _)| TabInfo { title }).collect();
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset;
    }

    pub fn set_color_scheme(&mut self, scheme: ColorScheme) {
        self.color_scheme = scheme;
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.cursor_style = style;
    }

    /// Get the surface/screen size in pixels.
    pub fn surface_size(&self) -> (u32, u32) {
        self.context.size
    }

    /// Get the cell size in integer pixels (for mouse encoder).
    pub fn cell_size(&self) -> (u32, u32) {
        let cs = self.atlas.cell_size();
        (cs.width.ceil() as u32, cs.height.ceil() as u32)
    }

    pub fn context(&self) -> &RenderContext {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut RenderContext {
        &mut self.context
    }
}
