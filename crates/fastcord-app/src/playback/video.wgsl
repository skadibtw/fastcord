struct Colors {
    red: vec4<f32>,
    green: vec4<f32>,
    blue: vec4<f32>,
};
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var linear_sampler: sampler;
@group(0) @binding(3) var<uniform> colors: Colors;

struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> Vertex {
    let positions = array<vec2<f32>, 6>(
        vec2(-1.0, 1.0), vec2(-1.0, -1.0), vec2(1.0, -1.0),
        vec2(-1.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0)
    );
    let position = positions[index];
    var out: Vertex;
    out.position = vec4(position, 0.0, 1.0);
    out.uv = vec2((position.x + 1.0) * 0.5, (1.0 - position.y) * 0.5);
    return out;
}

fn rgb(uv: vec2<f32>) -> vec3<f32> {
    let y = textureSample(luma, linear_sampler, uv).r;
    let cbcr = textureSample(chroma, linear_sampler, uv).rg;
    let yuv = vec4(y, cbcr, 1.0);
    return clamp(vec3(dot(colors.red, yuv), dot(colors.green, yuv), dot(colors.blue, yuv)), vec3(0.0), vec3(1.0));
}

@fragment
fn fragment_rgb(in: Vertex) -> @location(0) vec4<f32> {
    return vec4(rgb(in.uv), 1.0);
}

// Video RGB is already display-referred. An sRGB render target encodes its
// fragment output, so undo that transfer here rather than applying it twice.
@fragment
fn fragment_srgb(in: Vertex) -> @location(0) vec4<f32> {
    let nonlinear = rgb(in.uv);
    let linear = select(pow((nonlinear + vec3(0.055)) / vec3(1.055), vec3(2.4)), nonlinear / vec3(12.92), nonlinear <= vec3(0.04045));
    return vec4(linear, 1.0);
}
