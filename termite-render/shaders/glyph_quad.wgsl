struct Uniforms {
    viewport: vec2<f32>,
    _pad:     vec2<f32>,
}

struct VertexOutput {
    @builtin(position) pos:   vec4<f32>,
    @location(0)       uv:    vec2<f32>,
    @location(1)       color: vec4<f32>,
}

@group(0) @binding(0) var<uniform>  uniforms:       Uniforms;
@group(1) @binding(0) var           atlas_texture:  texture_2d<f32>;
@group(1) @binding(1) var           atlas_sampler:  sampler;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) dest_rect: vec4<f32>,
    @location(1) src_rect:  vec4<f32>,
    @location(2) color:     vec4<f32>,
) -> VertexOutput {
    let x  = dest_rect.x;
    let y  = dest_rect.y;
    let w  = dest_rect.z;
    let h  = dest_rect.w;
    let u  = src_rect.x;
    let v  = src_rect.y;
    let uw = src_rect.z;
    let vh = src_rect.w;

    var positions = array<vec2<f32>, 4>(
        vec2<f32>(x,     y    ),
        vec2<f32>(x + w, y    ),
        vec2<f32>(x,     y + h),
        vec2<f32>(x + w, y + h),
    );
    var uvs = array<vec2<f32>, 4>(
        vec2<f32>(u,      v     ),
        vec2<f32>(u + uw, v     ),
        vec2<f32>(u,      v + vh),
        vec2<f32>(u + uw, v + vh),
    );
    let idx = array<u32, 6>(0u, 1u, 2u, 1u, 3u, 2u);
    let p   = positions[idx[vi]];
    let uv2 = uvs[idx[vi]];

    let ndc_x =  p.x / uniforms.viewport.x * 2.0 - 1.0;
    let ndc_y = -p.y / uniforms.viewport.y * 2.0 + 1.0;

    var out: VertexOutput;
    out.pos   = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv    = uv2;
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
