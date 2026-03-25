//! Glyph atlas for efficient text rendering.
//!
//! Manages text rendering via glyphon/cosmic-text. Handles font loading,
//! text layout, and GPU texture atlas management for glyph caching.

use crate::color::ColorScheme;
use forgetty_vt::Screen;

/// Cell dimensions in pixels (floating point for sub-pixel positioning).
#[derive(Debug, Clone, Copy)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

/// Manages text rendering using glyphon's TextRenderer and TextAtlas.
pub struct GlyphAtlas {
    pub font_system: glyphon::FontSystem,
    pub swash_cache: glyphon::SwashCache,
    pub cache: glyphon::Cache,
    pub text_atlas: glyphon::TextAtlas,
    pub text_renderer: glyphon::TextRenderer,
    pub viewport: glyphon::Viewport,
    pub cell_size: CellSize,
    font_size: f32,
    line_height: f32,
    font_family: glyphon::Family<'static>,
}

impl GlyphAtlas {
    /// Create a new glyph atlas with the given font settings.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font_family: &str,
        font_size: f32,
    ) -> Self {
        let mut font_system = glyphon::FontSystem::new();

        // Resolve the font family — always use monospace for terminal
        let family = glyphon::Family::Monospace;

        let metrics = glyphon::Metrics::new(font_size, (font_size * 1.2).ceil());
        let cell_size = measure_cell_size(&mut font_system, family, metrics);
        let line_height = metrics.line_height;

        tracing::info!(
            "Font: family={font_family}, size={font_size}, cell={}x{}, line_height={line_height}",
            cell_size.width,
            cell_size.height,
        );

        let cache = glyphon::Cache::new(device);
        let mut text_atlas = glyphon::TextAtlas::new(device, queue, &cache, format);
        let text_renderer = glyphon::TextRenderer::new(
            &mut text_atlas,
            device,
            wgpu::MultisampleState::default(),
            None,
        );
        let viewport = glyphon::Viewport::new(device, &cache);

        Self {
            font_system,
            swash_cache: glyphon::SwashCache::new(),
            cache,
            text_atlas,
            text_renderer,
            viewport,
            cell_size,
            font_size,
            line_height,
            font_family: family,
        }
    }

    /// Get the cell dimensions in pixels.
    pub fn cell_size(&self) -> CellSize {
        self.cell_size
    }

    /// Prepare text for rendering by building text areas from the visible screen content.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &Screen,
        scroll_offset: usize,
        viewport_size: (u32, u32),
        color_scheme: &ColorScheme,
    ) -> Result<(), glyphon::PrepareError> {
        self.viewport
            .update(queue, glyphon::Resolution { width: viewport_size.0, height: viewport_size.1 });

        let visible_rows = (viewport_size.1 as f32 / self.cell_size.height).ceil() as usize;
        let metrics = glyphon::Metrics::new(self.font_size, self.line_height);

        // Build one buffer per visible row
        let mut buffers: Vec<glyphon::Buffer> = Vec::with_capacity(visible_rows);

        for vis_row in 0..visible_rows {
            let screen_row = vis_row + scroll_offset;
            if screen_row >= screen.rows() {
                break;
            }

            let row = screen.row(screen_row);
            let mut buffer = glyphon::Buffer::new(&mut self.font_system, metrics);
            buffer.set_size(
                &mut self.font_system,
                Some(viewport_size.0 as f32),
                Some(self.line_height),
            );

            // Build rich text spans with per-character colors and monospace font
            let char_strings: Vec<String> = row.iter().map(|c| c.character.to_string()).collect();

            let rich_text: Vec<(&str, glyphon::Attrs)> = char_strings
                .iter()
                .zip(row.iter())
                .map(|(s, cell)| {
                    let fg = if cell.attrs.inverse {
                        color_scheme.resolve_bg(cell.attrs.bg)
                    } else {
                        color_scheme.resolve_fg(cell.attrs.fg)
                    };

                    let color = glyphon::Color::rgba(fg[0], fg[1], fg[2], fg[3]);
                    let mut attrs = glyphon::Attrs::new().family(self.font_family).color(color);

                    if cell.attrs.bold {
                        attrs = attrs.weight(glyphon::Weight::BOLD);
                    }
                    if cell.attrs.italic {
                        attrs = attrs.style(glyphon::Style::Italic);
                    }

                    (s.as_str(), attrs)
                })
                .collect();

            buffer.set_rich_text(
                &mut self.font_system,
                rich_text,
                glyphon::Attrs::new().family(self.font_family),
                glyphon::Shaping::Advanced,
            );
            buffer.shape_until_scroll(&mut self.font_system, false);

            buffers.push(buffer);
        }

        // Build text areas from the buffers
        let text_areas: Vec<glyphon::TextArea<'_>> = buffers
            .iter()
            .enumerate()
            .map(|(vis_row, buffer)| glyphon::TextArea {
                buffer,
                left: 0.0,
                top: vis_row as f32 * self.cell_size.height,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: viewport_size.0 as i32,
                    bottom: viewport_size.1 as i32,
                },
                default_color: glyphon::Color::rgba(
                    color_scheme.foreground[0],
                    color_scheme.foreground[1],
                    color_scheme.foreground[2],
                    color_scheme.foreground[3],
                ),
                custom_glyphs: &[],
            })
            .collect();

        self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.text_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        )?;

        Ok(())
    }

    /// Render text into the render pass.
    pub fn render<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
    ) -> Result<(), glyphon::RenderError> {
        self.text_renderer.render(&self.text_atlas, &self.viewport, pass)
    }

    /// Trim the atlas to free unused texture space.
    pub fn trim(&mut self) {
        self.text_atlas.trim();
    }
}

/// Measure the width and height of a single monospace cell.
fn measure_cell_size(
    font_system: &mut glyphon::FontSystem,
    family: glyphon::Family<'_>,
    metrics: glyphon::Metrics,
) -> CellSize {
    // Create a buffer with a representative string to measure average monospace width
    let mut buffer = glyphon::Buffer::new(font_system, metrics);
    buffer.set_size(font_system, Some(10000.0), Some(metrics.line_height));
    buffer.set_text(
        font_system,
        "MMMMMMMMMM",
        glyphon::Attrs::new().family(family),
        glyphon::Shaping::Advanced,
    );
    buffer.shape_until_scroll(font_system, false);

    // Get the width from the layout run
    let mut total_width = 0.0;
    let mut glyph_count = 0;
    if let Some(run) = buffer.layout_runs().next() {
        for glyph in run.glyphs.iter() {
            total_width += glyph.w;
            glyph_count += 1;
        }
    }

    let width = if glyph_count > 0 {
        total_width / glyph_count as f32
    } else {
        // Fallback: estimate from font size
        metrics.font_size * 0.6
    };

    CellSize { width, height: metrics.line_height }
}
