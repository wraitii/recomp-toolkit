// DIVERGENCE(original): GPU float arithmetic and pow/normalization may differ
// from D3D8 hardware and the CPU reference by rounding. No color quantization
// is introduced before rasterization. Light order and diffuse alpha are kept.
struct LightingLight {
    position_type: vec4<f32>,
    direction: vec4<f32>,
    diffuse: vec4<f32>,
    ambient: vec4<f32>,
    attenuation_range: vec4<f32>,
    spot: vec4<f32>,
};
struct LightingUniform {
    world_view: mat4x4<f32>,
    normal: array<vec4<f32>, 3>,
    diffuse: vec4<f32>,
    ambient: vec4<f32>,
    emissive: vec4<f32>,
    global_ambient: vec4<f32>,
    flags: vec4<u32>,
    lights: array<LightingLight, 8>,
};
@group(0) @binding(5) var<uniform> lighting: LightingUniform;
fn lighting_normalize(v: vec3<f32>) -> vec3<f32> {
    let len = sqrt(dot(v, v));
    if (len == 0.0) { return vec3<f32>(0.0); }
    return v / len;
}
fn lighting_color(c: u32) -> vec4<f32> {
    return vec4<f32>(f32((c >> 16u) & 255u), f32((c >> 8u) & 255u),
                     f32(c & 255u), f32((c >> 24u) & 255u)) / 255.0;
}
fn evaluate_lighting(position: vec3<f32>, input_normal: vec3<f32>,
                     color: vec4<f32>, has_color: bool) -> vec4<f32> {
    let sources = lighting.flags.z;
    if (lighting.flags.x == 0u) {
        return clamp(select(lighting.diffuse, color, has_color && (sources & 8u) != 0u),
                     vec4<f32>(0.0), vec4<f32>(1.0));
    }
    let diffuse = select(lighting.diffuse, color, has_color && (sources & 1u) != 0u);
    let ambient = select(lighting.ambient, color, has_color && (sources & 2u) != 0u);
    let emissive = select(lighting.emissive, color, has_color && (sources & 4u) != 0u);
    var normal = vec3<f32>(dot(lighting.normal[0].xyz, input_normal),
                           dot(lighting.normal[1].xyz, input_normal),
                           dot(lighting.normal[2].xyz, input_normal));
    if (lighting.flags.y != 0u) { normal = lighting_normalize(normal); }
    let view_position = (lighting.world_view * vec4<f32>(position, 1.0)).xyz;
    var rgb = emissive.xyz + ambient.xyz * lighting.global_ambient.xyz;
    for (var i = 0u; i < lighting.flags.w; i += 1u) {
        let light = lighting.lights[i];
        var direction = -light.direction.xyz;
        var attenuation = 1.0;
        if (light.position_type.w != 3.0) {
            let delta = light.position_type.xyz - view_position;
            let distance = sqrt(dot(delta, delta));
            if (!(distance <= light.attenuation_range.w)) { continue; }
            direction = lighting_normalize(delta);
            let a = light.attenuation_range;
            attenuation = 1.0 / (a.x + a.y * distance + a.z * distance * distance);
        }
        if (light.position_type.w == 2.0) {
            let rho = -dot(direction, light.direction.xyz);
            var spot = 0.0;
            if (rho > light.spot.x) { spot = 1.0; }
            else if (rho > light.spot.y) {
                spot = pow((rho - light.spot.y) / (light.spot.x - light.spot.y), light.spot.z);
            }
            attenuation *= spot;
        }
        let lambert = max(dot(normal, direction), 0.0);
        rgb += attenuation * (ambient.xyz * light.ambient.xyz
                               + diffuse.xyz * light.diffuse.xyz * lambert);
    }
    return clamp(vec4<f32>(rgb, diffuse.w), vec4<f32>(0.0), vec4<f32>(1.0));
}
