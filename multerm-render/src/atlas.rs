use std::collections::HashMap;

use cosmic_text::{
    Attrs, Buffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    Weight,
};
use etagere::{AtlasAllocator, Size};

use crate::surface::GpuContext;

/// Cache key for a single rendered glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub ch: char,
    pub bold: bool,
    pub italic: bool,
    /// `scale * 100` — avoids float hashing.
    pub raster_scale: u32,
}

/// Atlas UV + pixel dimensions of a cached glyph.
#[derive(Clone, Copy, Debug)]
pub struct GlyphUV {
    /// Normalised UV origin and extent in the atlas texture.
    pub u: f32,
    pub v: f32,
    pub uw: f32,
    pub vh: f32,
    /// Pixel size of the glyph cell in the atlas.
    pub px_w: u32,
    pub px_h: u32,
}

pub struct GlyphAtlas {
    pub font_system: FontSystem,
    swash_cache: SwashCache,
    allocator: AtlasAllocator,
    pub texture: wgpu::Texture,
    pub texture_view: wgpu::TextureView,
    atlas_size: u32,
    cache: HashMap<GlyphKey, GlyphUV>,

    pub font_size: f32,
    pub raster_scale: f32,
}

impl GlyphAtlas {
    const INITIAL_SIZE: u32 = 1024;

    pub fn new(gpu: &GpuContext, font_size: f32) -> Self {
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let atlas_size = Self::INITIAL_SIZE;
        let (texture, texture_view) = Self::make_texture(&gpu.device, atlas_size);
        let allocator = AtlasAllocator::new(Size::new(atlas_size as i32, atlas_size as i32));

        Self {
            font_system,
            swash_cache,
            allocator,
            texture,
            texture_view,
            atlas_size,
            cache: HashMap::new(),
            font_size,
            raster_scale: 1.0,
        }
    }

    pub fn set_raster_scale(&mut self, scale: f32) {
        if (scale - self.raster_scale).abs() > 0.001 {
            self.raster_scale = scale;
            self.cache.clear();
            self.allocator =
                AtlasAllocator::new(Size::new(self.atlas_size as i32, self.atlas_size as i32));
        }
    }

    // ── Cell sizing ──────────────────────────────────────────────────────────

    pub fn cell_width_base(&self) -> f32 {
        self.font_size * 0.70 + 0.45
    }

    pub fn cell_height_base(&self) -> f32 {
        let line_height = self.font_size * 1.2;
        line_height.max(self.font_size * 1.12) * 1.12
    }

    pub fn cell_width(&self) -> f32 {
        self.cell_width_base() * self.raster_scale
    }

    pub fn cell_height(&self) -> f32 {
        self.cell_height_base() * self.raster_scale
    }

    // ── Glyph lookup / rasterise ─────────────────────────────────────────────

    /// Returns the `GlyphUV` for `key`, rasterising and uploading to the GPU
    /// texture if not already cached.
    pub fn get_or_rasterize(&mut self, gpu: &GpuContext, key: GlyphKey) -> Option<GlyphUV> {
        if let Some(uv) = self.cache.get(&key) {
            return Some(*uv);
        }
        self.rasterize(gpu, key)
    }

    fn rasterize(&mut self, gpu: &GpuContext, key: GlyphKey) -> Option<GlyphUV> {
        let physical_font = self.font_size * self.raster_scale;
        let line_height = physical_font * 1.2;

        let cell_w = self.cell_width() as u32;
        let cell_h = self.cell_height() as u32;
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);

        let metrics = Metrics::new(physical_font, line_height);
        let mut buf = Buffer::new(&mut self.font_system, metrics);
        buf.set_size(
            &mut self.font_system,
            Some(cell_w as f32),
            Some(cell_h as f32),
        );

        let attrs = Attrs::new()
            .family(Family::Monospace)
            .weight(if key.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if key.italic {
                Style::Italic
            } else {
                Style::Normal
            });

        let s = key.ch.to_string();
        buf.set_text(&mut self.font_system, &s, attrs, Shaping::Basic);
        buf.shape_until_scroll(&mut self.font_system, false);

        // Rasterise into an alpha-only pixel buffer.
        let mut alpha = vec![0u8; (cell_w * cell_h) as usize];
        buf.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            CosmicColor::rgb(255, 255, 255),
            |x, y, _w, _h, color| {
                if x < 0 || y < 0 {
                    return;
                }
                let px = x as u32;
                let py = y as u32;
                if px < cell_w && py < cell_h {
                    alpha[(py * cell_w + px) as usize] = color.a();
                }
            },
        );

        // Allocate space in the atlas.
        let alloc = match self
            .allocator
            .allocate(Size::new(cell_w as i32, cell_h as i32))
        {
            Some(a) => a,
            None => {
                // Atlas full — clear and try again.
                self.cache.clear();
                self.allocator =
                    AtlasAllocator::new(Size::new(self.atlas_size as i32, self.atlas_size as i32));
                self.allocator
                    .allocate(Size::new(cell_w as i32, cell_h as i32))?
            }
        };

        let origin = alloc.rectangle.min;
        let ox = origin.x as u32;
        let oy = origin.y as u32;

        // Upload alpha data to the R8Unorm atlas texture.
        gpu.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ox, y: oy, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &alpha,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(cell_w),
                rows_per_image: Some(cell_h),
            },
            wgpu::Extent3d {
                width: cell_w,
                height: cell_h,
                depth_or_array_layers: 1,
            },
        );

        let uv = GlyphUV {
            u: ox as f32 / self.atlas_size as f32,
            v: oy as f32 / self.atlas_size as f32,
            uw: cell_w as f32 / self.atlas_size as f32,
            vh: cell_h as f32 / self.atlas_size as f32,
            px_w: cell_w,
            px_h: cell_h,
        };
        self.cache.insert(key, uv);
        Some(uv)
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_texture(device: &wgpu::Device, size: u32) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph_atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }
}
