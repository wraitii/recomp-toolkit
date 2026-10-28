use super::*;

const WORLDVIEW_ROW: usize = 0;
const WVP_ROW: usize = 4;
const NORMAL_ROW: usize = 8;
const VIEWPORT_ROW: usize = 11;
const DEPTH_ROW: usize = 12;
const MATERIAL_ROW: usize = 13;
const POWER_FOG_ROW: usize = 17;
const AMBIENT_ROW: usize = 18;
const TEXMATRIX_ROW: usize = 19;
const FLAG_ROW: usize = TEXMATRIX_ROW + 32;
const FLAG_ROWS: usize = 11;
const LIGHT_ROW: usize = FLAG_ROW + FLAG_ROWS;
const LIGHT_SIZE: usize = 7;
const MAX_LIGHTS: usize = 8;
const UNIFORM_ROWS: usize = LIGHT_ROW + MAX_LIGHTS * LIGHT_SIZE;

const FLAG_NEEDS_NORMAL: usize = 0;
const FLAG_LIGHTING: usize = 1;
const FLAG_SPECULAR: usize = 2;
const FLAG_LOCAL_VIEWER: usize = 3;
const FLAG_NORMALIZE: usize = 4;
const FLAG_SOURCES: usize = 5;
const FLAG_FOG: usize = 9;
const FLAG_LIGHT_COUNT: usize = 10;
const FLAG_STAGES: usize = 11;

fn source_count(element_type: u8) -> u32 {
    match element_type {
        D3DDECLTYPE_FLOAT1 => 1,
        D3DDECLTYPE_FLOAT2
        | D3DDECLTYPE_SHORT2
        | D3DDECLTYPE_SHORT2N
        | D3DDECLTYPE_USHORT2N
        | D3DDECLTYPE_FLOAT16_2 => 2,
        D3DDECLTYPE_FLOAT3 | D3DDECLTYPE_UDEC3 | D3DDECLTYPE_DEC3N => 3,
        _ => 4,
    }
}

fn material_source(state: &D3d9State, id: u32) -> u32 {
    if rs(state, 141) == 0 {
        return 0;
    }
    match rs(state, id) {
        1 => 1,
        2 => 2,
        _ => 0,
    }
}

pub(super) fn prepare(
    state: &D3d9State,
    decl: &[VertexElement],
    inputs: &mut Vec<LinkedInput>,
    uniforms: &mut Vec<f32>,
) -> Result<Option<[u32; 5]>, RenderError> {
    inputs.clear();
    if [151, 152, 167].iter().any(|&id| rs(state, id) != 0) {
        return Ok(None);
    }
    let find = |usage: u8, index: u8| {
        decl.iter()
            .find(|e| e.usage == usage && e.usage_index == index)
    };
    let Some(position) = find(9, 0).or_else(|| find(0, 0)) else {
        return Ok(None);
    };
    let transformed = position.usage == 9;
    let mut link = |location: u32, element: &VertexElement| -> bool {
        if native_vertex_format(element.element_type).is_none() {
            return false;
        }
        inputs.push(LinkedInput {
            location,
            stream: element.stream,
            offset: element.offset,
            element_type: element.element_type,
        });
        true
    };
    if !link(0, position) {
        return Ok(None);
    }
    let lighting = !transformed && rs(state, 137) != 0;
    let needs_normal = rs(state, 137) != 0
        || (0..8).any(|stage| matches!(tss(state, stage, 11) & 0xffff0000, 0x10000 | 0x30000));
    let mut key = [u32::from(transformed), 0, 0, 0, 0];
    for (slot, (location, usage, index)) in [(1, 10, 0), (2, 10, 1)].into_iter().enumerate() {
        let present = match find(usage, index) {
            Some(element) => {
                if !link(location, element) {
                    return Ok(None);
                }
                true
            }
            None => false,
        };
        key[1 + slot] = u32::from(present);
    }
    let normal = if needs_normal { find(3, 0) } else { None };
    if let Some(element) = normal {
        if !link(3, element) {
            return Ok(None);
        }
    }
    key[3] = u32::from(normal.is_some());
    let mut flags = [0u32; FLAG_ROWS * 4];
    flags[FLAG_NEEDS_NORMAL] = u32::from(needs_normal);
    flags[FLAG_LIGHTING] = u32::from(lighting);
    flags[FLAG_SPECULAR] = u32::from(rs(state, 29) != 0);
    flags[FLAG_LOCAL_VIEWER] = u32::from(rs(state, 142) != 0);
    flags[FLAG_NORMALIZE] = u32::from(rs(state, 143) != 0);
    flags[FLAG_SOURCES..FLAG_SOURCES + 4].copy_from_slice(&[
        material_source(state, 145),
        material_source(state, 147),
        material_source(state, 146),
        material_source(state, 148),
    ]);
    flags[FLAG_FOG] = if transformed { 0 } else { rs(state, 140) };
    let mut texcoord_mask = 0u32;
    let mut stage_matrices = [[0.0f32; 16]; 8];
    for stage in 0..8u32 {
        let coord = tss(state, stage, 11);
        let mode = coord & 0xffff0000;
        if !matches!(mode, 0 | 0x10000 | 0x20000 | 0x30000) {
            return Ok(None);
        }
        let mut source = 0;
        let mut count = 4;
        if mode == 0 {
            let usage = (coord & 0xffff) as u8;
            if let Some(element) = (usage < 8).then(|| find(5, usage)).flatten() {
                if texcoord_mask & (1u32 << usage) == 0 {
                    if !link(4 + u32::from(usage), element) {
                        return Ok(None);
                    }
                    texcoord_mask |= 1u32 << usage;
                }
                source = u32::from(usage) + 1;
                count = source_count(element.element_type);
            } else if usage >= 8 && find(5, usage).is_some() {
                return Ok(None);
            }
        }
        let transform = !transformed && tss(state, stage, 24) & 255 != 0;
        if transform {
            stage_matrices[stage as usize] = state.transform(16 + stage)?;
        }
        flags[FLAG_STAGES + 4 * stage as usize..][..4].copy_from_slice(&[
            mode >> 16,
            source,
            u32::from(transform),
            count,
        ]);
    }
    key[4] = texcoord_mask;
    let world = state.transform(256)?;
    let view = state.transform(2)?;
    let projection = state.transform(3)?;
    let worldview = multiply_matrices(&world, &view);
    let wvp = multiply_matrices(&worldview, &projection);
    let normal_matrix = if !transformed && needs_normal {
        let (a, b, c, d, e, f, g, h, i) = (
            worldview[0],
            worldview[1],
            worldview[2],
            worldview[4],
            worldview[5],
            worldview[6],
            worldview[8],
            worldview[9],
            worldview[10],
        );
        let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
        if det == 0.0 || !det.is_finite() {
            return Ok(None);
        }
        inverse3(&worldview)?
    } else {
        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
    };
    let raw_lights = if lighting {
        state.enabled_lights()
    } else {
        Vec::new()
    };
    if raw_lights.len() > MAX_LIGHTS {
        return Ok(None);
    }
    flags[FLAG_LIGHT_COUNT] = raw_lights.len() as u32;
    let vp = state.viewport();
    let u = uniforms;
    u.clear();
    u.reserve(UNIFORM_ROWS * 4);
    u.extend_from_slice(&worldview);
    u.extend_from_slice(&wvp);
    for row in normal_matrix {
        u.extend_from_slice(&[row[0], row[1], row[2], 0.0]);
    }
    u.extend_from_slice(&[vp.x as f32, vp.y as f32, vp.width as f32, vp.height as f32]);
    u.extend_from_slice(&[vp.min_z, vp.max_z - vp.min_z, 0.0, 0.0]);
    let material = state.material().map(f32::from_bits);
    u.extend_from_slice(&material[..16]);
    u.extend_from_slice(&[
        material[16],
        f32::from_bits(rs(state, 38)),
        f32::from_bits(rs(state, 36)),
        f32::from_bits(rs(state, 37)),
    ]);
    u.extend_from_slice(&color(rs(state, 139)));
    for matrix in &stage_matrices {
        u.extend_from_slice(matrix);
    }
    u.extend(flags.iter().map(|&value| f32::from_bits(value)));
    for raw in &raw_lights {
        let values = raw.map(f32::from_bits);
        let kind = raw[0];
        if kind != 3 && !(values[21] > 0.0 && values[22] >= 0.0 && values[23] >= 0.0) {
            return Ok(None);
        }
        let view_direction = transform_point(&view, [values[16], values[17], values[18], 0.0]);
        let position = if kind == 3 {
            [0.0; 4]
        } else {
            transform_point(&view, [values[13], values[14], values[15], 1.0])
        };
        let direction = match kind {
            2 => normalize3([view_direction[0], view_direction[1], view_direction[2]]),
            3 => normalize3([-view_direction[0], -view_direction[1], -view_direction[2]]),
            _ => [0.0; 3],
        };
        let (inner, outer) = if kind == 2 {
            ((values[24] * 0.5).cos(), (values[25] * 0.5).cos())
        } else {
            (0.0, 0.0)
        };
        u.extend_from_slice(&values[1..13]);
        u.extend_from_slice(&[position[0], position[1], position[2], values[19]]);
        u.extend_from_slice(&[direction[0], direction[1], direction[2], values[20]]);
        u.extend_from_slice(&[values[21], values[22], values[23], f32::from_bits(kind)]);
        u.extend_from_slice(&[inner, outer, 0.0, 0.0]);
    }
    u.resize(UNIFORM_ROWS * 4, 0.0);
    Ok(Some(key))
}

pub(super) fn source(key: &[u32]) -> String {
    let transformed = key[0] != 0;
    let (c0, c1) = (key[1] != 0, key[2] != 0);
    let has_normal = key[3] != 0;
    let mask = key[4];
    let linked = |usage: u32| mask & (1 << usage) != 0;
    let mut s = String::from(V2F);
    s += shader_interface::VERTEX_PARAMETERS;
    s += "@group(0) @binding(1) var<uniform> vsp:VSP;\n";
    s += &format!(
        "struct FFU {{ m:array<vec4<f32>,{UNIFORM_ROWS}> }}\n@group(0) @binding(0) var<uniform> u:FFU;\n"
    );
    s += "fn xf(b:u32,v:vec4<f32>)->vec4<f32>{return v.x*u.m[b]+v.y*u.m[b+1u]+v.z*u.m[b+2u]+v.w*u.m[b+3u];}\n";
    s +=
        "fn nz(v:vec3<f32>)->vec3<f32>{let l=length(v);return select(vec3<f32>(0.0),v/l,l>0.0);}\n";
    s += "fn pw(x:f32,y:f32)->f32{return select(pow(x,y),select(0.0,1.0,y==0.0),x<=0.0);}\n";
    s += &format!(
        "fn flag(i:u32)->u32{{let r=bitcast<vec4<u32>>(u.m[{FLAG_ROW}u+i/4u]);return r[i%4u];}}\n"
    );
    s += "fn comp(t:vec4<f32>,c:u32)->vec4<f32>{let i=vec4<u32>(0u,1u,2u,3u);return select(select(vec4<f32>(0.0),vec4<f32>(1.0),i==vec4<u32>(c)),t,i<vec4<u32>(c));}\n";
    s += "struct FfIn {\n@location(0) p:vec4<f32>,\n";
    if c0 {
        s += "@location(1) c0:vec4<f32>,\n";
    }
    if c1 {
        s += "@location(2) c1:vec4<f32>,\n";
    }
    if has_normal {
        s += "@location(3) n:vec4<f32>,\n";
    }
    for usage in (0..8).filter(|&usage| linked(usage)) {
        s += &format!("@location({}) t{usage}:vec4<f32>,\n", 4 + usage);
    }
    s += "}\n";
    s += "fn texcoord(v:FfIn,usage:u32)->vec4<f32>{var r=vec4<f32>(0.0,0.0,0.0,1.0);switch usage{";
    for usage in (0..8).filter(|&usage| linked(usage)) {
        s += &format!("case {usage}u:{{r=v.t{usage};}}");
    }
    s += "default:{}}return r;}\n";
    s += "fn pick(s:u32,m:vec4<f32>,v0:vec4<f32>,v1:vec4<f32>)->vec4<f32>{var r=m;";
    if c0 {
        s += "if s==1u{r=v0;}";
    }
    if c1 {
        s += "if s==2u{r=v1;}";
    }
    s += "return r;}\n";
    s += "fn tc(v:FfIn,n:vec3<f32>,viewp:vec4<f32>,s:u32)->vec4<f32>{";
    s += &format!(
        "let f={FLAG_STAGES}u+4u*s;let mode=flag(f);let src=flag(f+1u);let count=flag(f+3u);var t=vec4<f32>(0.0,0.0,0.0,1.0);"
    );
    s += "switch mode{case 0u:{if src!=0u{t=texcoord(v,src-1u);}}case 1u:{t=vec4<f32>(n,1.0);}case 2u:{t=viewp;}default:{let e=nz(viewp.xyz);t=vec4<f32>(e-2.0*dot(e,n)*n,1.0);}}\n";
    s += &format!("if flag(f+2u)!=0u{{t=xf({TEXMATRIX_ROW}u+4u*s,comp(t,count));}}return t;}}\n");
    s += "@vertex fn vs_main(v:FfIn)->V2F {var o:V2F;\nlet p=v.p;\n";
    s += &format!(
        "let v0=vec4<f32>({});let v1=vec4<f32>({});\n",
        if c0 { "v.c0" } else { "1.0" },
        if c1 { "v.c1" } else { "0.0" }
    );
    if transformed {
        s += &format!("let w=1.0/p.w;let vp=u.m[{VIEWPORT_ROW}u];let dz=u.m[{DEPTH_ROW}u].y;\n");
        s += &format!(
            "let clip=vec4<f32>(((p.x-vp.x)/vp.z*2.0-1.0)*w,(1.0-(p.y-vp.y)/vp.w*2.0)*w,select((p.z-u.m[{DEPTH_ROW}u].x)/dz*w,0.0,dz==0.0),w);\n"
        );
        s += "let viewp=vec4<f32>(p.xyz,1.0);\n";
    } else {
        s += &format!("let viewp=xf({WORLDVIEW_ROW}u,p);let clip=xf({WVP_ROW}u,p);\n");
    }
    s += if has_normal {
        "let n0=v.n.xyz;\n"
    } else {
        "let n0=vec3<f32>(0.0);\n"
    };
    s += &format!("var n=vec3<f32>(0.0);\nif flag({FLAG_NEEDS_NORMAL}u)!=0u{{");
    if transformed {
        s += "n=n0;";
    } else {
        s += &format!(
            "n=vec3<f32>(dot(u.m[{}u].xyz,n0),dot(u.m[{}u].xyz,n0),dot(u.m[{}u].xyz,n0));if flag({FLAG_NORMALIZE}u)!=0u{{n=nz(n);}}",
            NORMAL_ROW,
            NORMAL_ROW + 1,
            NORMAL_ROW + 2
        );
    }
    s += "}\n";
    s += &format!("if flag({FLAG_LIGHTING}u)!=0u{{");
    for (i, name) in ["md", "ma", "ms", "me"].into_iter().enumerate() {
        s += &format!(
            "let {name}=pick(flag({}u),u.m[{}u],v0,v1);",
            FLAG_SOURCES + i,
            MATERIAL_ROW + i
        );
    }
    s += &format!(
        "let eye=select(vec3<f32>(0.0,0.0,1.0),nz(-viewp.xyz),flag({FLAG_LOCAL_VIEWER}u)!=0u);let spec=flag({FLAG_SPECULAR}u)!=0u;"
    );
    s += &format!("var r0=me.rgb+ma.rgb*u.m[{AMBIENT_ROW}u].rgb;var r1=vec3<f32>(0.0);");
    s += &format!(
        "for(var i=0u;i<flag({FLAG_LIGHT_COUNT}u);i+=1u){{let b={LIGHT_ROW}u+{LIGHT_SIZE}u*i;"
    );
    s += "let kind=bitcast<u32>(u.m[b+5u].w);var l=vec3<f32>(0.0);var att=1.0;";
    s += "if kind==3u{l=u.m[b+4u].xyz;}else{let d=u.m[b+3u].xyz-viewp.xyz;let dist=length(d);let ka=u.m[b+5u];l=nz(d);att=select(1.0/(ka.x+ka.y*dist+ka.z*dist*dist),0.0,dist>u.m[b+3u].w);}";
    s += "if kind==2u{let rho=-dot(l,u.m[b+4u].xyz);let cs=u.m[b+6u];let fo=u.m[b+4u].w;att*=select(select(pw((rho-cs.y)/(cs.x-cs.y),fo),0.0,rho<=cs.y),1.0,rho>=cs.x);}";
    s += &format!(
        "let nl=max(dot(n,l),0.0);let hl=select(0.0,pw(max(dot(n,nz(l+eye)),0.0),u.m[{POWER_FOG_ROW}u].x),nl>0.0&&spec);"
    );
    s += "r0+=att*(ma.rgb*u.m[b+2u].rgb+md.rgb*u.m[b].rgb*nl);r1+=att*ms.rgb*u.m[b+1u].rgb*hl;}";
    s += "o.d0=clamp(vec4<f32>(r0,md.a),vec4<f32>(0.0),vec4<f32>(1.0));o.d1=clamp(vec4<f32>(r1,0.0),vec4<f32>(0.0),vec4<f32>(1.0));}else{o.d0=v0;o.d1=v1;}\n";
    for stage in 0..8 {
        s += &format!("o.t{stage}=tc(v,n,viewp,{stage}u);\n");
    }
    if transformed {
        s += if c1 {
            "o.fog=v.c1.a;\n"
        } else {
            "o.fog=1.0;\n"
        };
    } else {
        s += &format!("let z=abs(viewp.z);let fp=u.m[{POWER_FOG_ROW}u];");
        s += &format!(
            "switch flag({FLAG_FOG}u){{case 1u:{{o.fog=exp(-fp.y*z);}}case 2u:{{o.fog=exp(-(fp.y*z)*(fp.y*z));}}case 3u:{{o.fog=clamp((fp.w-z)/(fp.w-fp.z),0.0,1.0);}}default:{{o.fog=1.0;}}}}\n"
        );
    }
    s += "o.fogz=viewp.z;o.pos=vec4<f32>(clip.xy+vsp.halfpix.xy*clip.w,clip.zw);return o;}\n";
    s
}
