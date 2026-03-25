struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

struct Uniforms {
    viewport_size: vec2<f32>,
};

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

// Unit quad vertices (triangle strip: 4 vertices = 2 triangles)
var<private> QUAD_VERTICES: array<vec2<f32>, 4> = array<vec2<f32>, 4>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 1.0),
);

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    let vertex = QUAD_VERTICES[vertex_index];

    // Transform from pixel coordinates to clip space (-1 to 1)
    let pixel_pos = pos + vertex * size;
    let clip_pos = vec2<f32>(
        (pixel_pos.x / uniforms.viewport_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.viewport_size.y) * 2.0,
    );

    out.position = vec4<f32>(clip_pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
