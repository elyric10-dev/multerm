/// Axis-aligned rectangle in pixel space.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn as_array(&self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }
}
