use multerm_vt::cell::Color;

/// Default terminal foreground colour (#d4d4d8).
pub const DEFAULT_FG: [f32; 4] = [0.831, 0.831, 0.847, 1.0];
/// Default terminal background colour (opaque black).
pub const DEFAULT_BG: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Convert a `multerm_vt::Color` to linear RGBA.
pub fn color_to_linear(c: Color, is_fg: bool) -> [f32; 4] {
    match c {
        Color::Default => {
            if is_fg {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            }
        }
        Color::Indexed(i) => {
            let [r, g, b] = ansi_indexed_to_rgb(i);
            srgb_to_linear(r, g, b, 255)
        }
        Color::Rgb(r, g, b) => srgb_to_linear(r, g, b, 255),
    }
}

/// Map a 256-colour index to (r, g, b) in 0–255 sRGB.
pub fn ansi_indexed_to_rgb(i: u8) -> [u8; 3] {
    // Classic 16 ANSI colors
    static ANSI16: [[u8; 3]; 16] = [
        [0, 0, 0],       // 0 Black
        [170, 0, 0],     // 1 Red
        [0, 170, 0],     // 2 Green
        [170, 170, 0],   // 3 Yellow
        [0, 0, 170],     // 4 Blue
        [170, 0, 170],   // 5 Magenta
        [0, 170, 170],   // 6 Cyan
        [170, 170, 170], // 7 White
        [85, 85, 85],    // 8 Bright Black (dark grey)
        [255, 85, 85],   // 9 Bright Red
        [85, 255, 85],   // 10 Bright Green
        [255, 255, 85],  // 11 Bright Yellow
        [85, 85, 255],   // 12 Bright Blue
        [255, 85, 255],  // 13 Bright Magenta
        [85, 255, 255],  // 14 Bright Cyan
        [255, 255, 255], // 15 Bright White
    ];

    if (i as usize) < 16 {
        return ANSI16[i as usize];
    }

    // 6×6×6 colour cube: indices 16–231
    if i < 232 {
        let n = i - 16;
        let b = n % 6;
        let g = (n / 6) % 6;
        let r = n / 36;
        let f = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
        return [f(r), f(g), f(b)];
    }

    // Grayscale ramp: indices 232–255
    let v = 8 + (i - 232) * 10;
    [v, v, v]
}

fn srgb_to_linear(r: u8, g: u8, b: u8, a: u8) -> [f32; 4] {
    [
        srgb_channel(r),
        srgb_channel(g),
        srgb_channel(b),
        a as f32 / 255.0,
    ]
}

fn srgb_channel(v: u8) -> f32 {
    let f = v as f32 / 255.0;
    if f <= 0.04045 {
        f / 12.92
    } else {
        ((f + 0.055) / 1.055).powf(2.4)
    }
}
