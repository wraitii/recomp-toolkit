fn d9_nrm(a: vec4<f32>) -> vec4<f32> { let l = length(a.xyz); if (l > 0.0) { return a / l; } return vec4<f32>(0.0); }
