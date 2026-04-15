use bytemuck::{Pod, Zeroable};

/// Per-instance data for background quad rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct BgQuadInstance {
    /// Pixel-space rect: x, y, width, height.
    pub rect:  [f32; 4],
    /// Linear-space RGBA colour.
    pub color: [f32; 4],
}

/// Per-instance data for glyph quad rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct GlyphInstance {
    /// Destination rect in pixel space: x, y, width, height.
    pub dest_rect: [f32; 4],
    /// Source rect in normalised atlas UV space: u, v, uw, vh.
    pub src_rect:  [f32; 4],
    /// Linear-space RGBA foreground colour.
    pub color:     [f32; 4],
}
