struct Uniforms {
    viewport: vec2<f32>,
    _pad:     vec2<f32>,
}

struct VertexOutput {
    @builtin(position) pos:   vec4<f32>,
    @location(0)       color: vec4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) rect:  vec4<f32>,
    @location(1) color: vec4<f32>,
) -> VertexOutput {
    // Build a quad from 6 vertex indices (two CCW triangles).
    let x = rect.x;
    let y = rect.y;
    let w = rect.z;
    let h = rect.w;

    var corners = array<vec2<f32>, 4>(
        vec2<f32>(x,     y    ),
        vec2<f32>(x + w, y    ),
        vec2<f32>(x,     y + h),
        vec2<f32>(x + w, y + h),
    );
    // Triangle strip: 0,1,2 / 1,3,2
    let idx = array<u32, 6>(0u, 1u, 2u, 1u, 3u, 2u);
    let p   = corners[idx[vi]];

    let ndc_x =  p.x / uniforms.viewport.x * 2.0 - 1.0;
    let ndc_y = -p.y / uniforms.viewport.y * 2.0 + 1.0;

    var out: VertexOutput;
    out.pos   = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
