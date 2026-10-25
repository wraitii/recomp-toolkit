use crate::RenderError;
use crate::shader::program::*;
pub use crate::shader::program::{DclIn, Dst, Inst, Program, Src};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default)]
pub struct Variant {
    pub unbound: u16,
    pub cube: u16,
    pub volume: u16,
    pub projected: u16,
    pub depth: u16,
    pub alpha: u16,
    pub raw_inputs: u16,
}

const PRELUDE: &str = r#"diagnostic(off, derivative_uniformity);
fn d9_dz(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(v.x / v.z, v.y / v.z, v.z, v.w); }
fn d9_dw(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(v.x / v.w, v.y / v.w, v.z, v.w); }
fn d9_lit(a: vec4<f32>) -> vec4<f32> { var o = vec4<f32>(1.0, max(a.x, 0.0), 0.0, 1.0); if (a.x > 0.0 && a.y > 0.0) { o.z = pow(a.y, clamp(a.w, -127.9961, 127.9961)); } return o; }
fn d9_nrm(a: vec4<f32>) -> vec4<f32> { let l = length(a.xyz); if (l > 0.0) { return a / l; } return vec4<f32>(0.0); }
fn d9_round(v: vec4<f32>) -> vec4<f32> { return sign(v) * floor(abs(v) + 0.5); }
fn d9_alpha(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(0.0, 0.0, 0.0, v.r); }
fn d9_shadow(t: texture_depth_2d, sm: sampler_comparison, c: vec4<f32>) -> vec4<f32> { let r = textureSampleCompareLevel(t, sm, c.xy, clamp(c.z, 0.0, 1.0)); return vec4<f32>(r, r, r, 1.0); }
struct VSP { halfpix: vec4<f32>, ascale: array<vec4<f32>, 16> }
struct PSP { alpha_ref: f32, alpha_func: i32, fog_mode: i32, pad0: i32, fog_color: vec4<f32>, fog_start: f32, fog_end: f32, fog_density: f32, pad1: f32 }
struct V2F { @builtin(position) pos: vec4<f32>, @location(0) d0: vec4<f32>, @location(1) d1: vec4<f32>, @location(2) t0: vec4<f32>, @location(3) t1: vec4<f32>, @location(4) t2: vec4<f32>, @location(5) t3: vec4<f32>, @location(6) t4: vec4<f32>, @location(7) t5: vec4<f32>, @location(8) t6: vec4<f32>, @location(9) t7: vec4<f32>, @location(10) fog: f32, @location(11) fogz: f32 }
"#;

const COMP: &[u8; 4] = b"xyzw";

fn swizzle(swz: u32) -> String {
    (0..4)
        .map(|k| COMP[((swz >> (2 * k)) & 3) as usize] as char)
        .collect()
}

struct Gen<'a> {
    p: &'a Program,
    variant: &'a Variant,
    body: String,
    stages: BTreeSet<u32>,
    failed: Option<String>,
    seq: u32,
}

impl<'a> Gen<'a> {
    fn new(p: &'a Program, variant: &'a Variant) -> Self {
        Self {
            p,
            variant,
            body: String::new(),
            stages: BTreeSet::new(),
            failed: None,
            seq: 0,
        }
    }

    fn fail(&mut self, cause: impl Into<String>) {
        if self.failed.is_none() {
            self.failed = Some(cause.into());
        }
    }

    fn const_limit(&self) -> u32 {
        self.p.const_limit()
    }

    fn stage_kind(&self, stage: u32) -> u32 {
        match self.p.samplers.get(&stage).copied() {
            Some(kind) => kind,
            None if self.variant.cube & (1u16 << stage) != 0 => S_CUBE,
            None if self.variant.volume & (1u16 << stage) != 0 => S_VOLUME,
            None => 0,
        }
    }

    fn base(&mut self, s: &Src) -> String {
        let pixel = self.p.pixel;
        match s.ty {
            R_TEMP => format!("r[{}]", s.index),
            R_INPUT => {
                if !pixel {
                    let declared = self
                        .p
                        .inputs
                        .iter()
                        .any(|d| d.ty == R_INPUT && d.index == s.index);
                    if !declared {
                        self.fail(format!(
                            "vertex input v{} is read without a declaration",
                            s.index
                        ));
                    }
                    format!("vv[{}]", s.index)
                } else {
                    format!("iv[{}]", s.index)
                }
            }
            R_CONST => {
                let bank = if pixel { "pcb" } else { "vcb" };
                if s.rel {
                    let index = format!(
                        "clamp(i32(a0.{}) + {}, 0, {})",
                        COMP[s.rel_comp as usize] as char,
                        s.index,
                        self.const_limit() - 1
                    );
                    if self.p.defs.is_empty() {
                        format!("{bank}[{index}]")
                    } else {
                        format!("d9_const({index})")
                    }
                } else if self.p.defs.contains_key(&s.index) {
                    format!("d9_c{}()", s.index)
                } else {
                    format!("{bank}[{}]", s.index)
                }
            }
            R_ADDR => {
                if !pixel {
                    "a0".into()
                } else {
                    format!("t[{}]", s.index)
                }
            }
            R_CONSTINT => match self.p.idefs.get(&s.index) {
                Some(v) => format!("vec4<f32>({}.0, {}.0, {}.0, {}.0)", v[0], v[1], v[2], v[3]),
                None => {
                    self.fail(format!("integer constant i{} has no defi", s.index));
                    "vec4<f32>(0.0)".into()
                }
            },
            R_CONSTBOOL => match self.p.bdefs.get(&s.index) {
                Some(true) => "vec4<f32>(1.0)".into(),
                Some(false) => "vec4<f32>(0.0)".into(),
                None => {
                    self.fail(format!("boolean constant b{} has no defb", s.index));
                    "vec4<f32>(0.0)".into()
                }
            },
            _ => {
                self.fail(format!("source register type {}", s.ty));
                "junk".into()
            }
        }
    }

    fn src(&mut self, s: &Src) -> String {
        let b = self.base(s);
        let v = format!("({b}).{}", swizzle(s.swz));
        match s.md {
            0 => v,
            1 => format!("(-{v})"),
            2 => format!("({v} - vec4<f32>(0.5))"),
            3 => format!("(-({v} - vec4<f32>(0.5)))"),
            4 => format!("(2.0 * {v} - vec4<f32>(1.0))"),
            5 => format!("(-(2.0 * {v} - vec4<f32>(1.0)))"),
            6 => format!("(vec4<f32>(1.0) - {v})"),
            7 => format!("(2.0 * {v})"),
            8 => format!("(-(2.0 * {v}))"),
            9 => format!("d9_dz({v})"),
            10 => format!("d9_dw({v})"),
            11 => format!("abs({v})"),
            12 => format!("(-abs({v}))"),
            13 => format!("select(vec4<f32>(1.0), vec4<f32>(0.0), {v} != vec4<f32>(0.0))"),
            _ => {
                self.fail(format!("source modifier {}", s.md));
                v
            }
        }
    }

    fn s(&mut self, inst: &Inst, k: usize) -> String {
        self.src(&inst.src[k])
    }

    fn dst_name(&mut self, d: &Dst) -> String {
        let pixel = self.p.pixel;
        match d.ty {
            R_TEMP => format!("r[{}]", d.index),
            R_ADDR => {
                if !pixel {
                    "a0".into()
                } else {
                    format!("t[{}]", d.index)
                }
            }
            R_RASTOUT => match d.index {
                0 => "opos".into(),
                1 => "ofog".into(),
                _ => {
                    self.fail("point size output is not implemented");
                    "junk".into()
                }
            },
            R_ATTROUT => format!("od[{}]", d.index),
            R_TEXCRDOUT => {
                if self.p.major >= 3 {
                    format!("ov[{}]", d.index)
                } else {
                    format!("ot[{}]", d.index)
                }
            }
            R_COLOROUT => {
                if d.index != 0 {
                    // DIVERGENCE(original): writes to oC1-oC3 are dropped; multiple render targets are not implemented.
                    eprintln!(
                        "d3d9: dropping color output oC{}; multiple render targets are not implemented",
                        d.index
                    );
                }
                format!("oc[{}]", d.index)
            }
            R_DEPTHOUT => {
                self.fail("pixel shader depth output is not implemented");
                "junk".into()
            }
            _ => {
                self.fail(format!("destination register type {}", d.ty));
                "junk".into()
            }
        }
    }

    fn dest_value(&mut self, d: &Dst, v: &str) -> String {
        let mut v = format!("({v})");
        if d.shift > 0 {
            v = format!("({v} * {}.0)", 1u32 << d.shift);
        } else if d.shift < 0 {
            v = format!("({v} / {}.0)", 1u32 << (-d.shift));
        }
        if d.saturate {
            v = format!("clamp({v}, vec4<f32>(0.0), vec4<f32>(1.0))");
        }
        if !self.p.pixel && d.ty == R_ADDR {
            v = format!("d9_round({v})");
        }
        v
    }

    fn commit(&mut self, d: &Dst, temp: &str) {
        if d.mask == 0 {
            return;
        }
        let name = self.dst_name(d);
        if d.mask == 0xf {
            self.body += &format!("{name} = {temp};\n");
            return;
        }
        let mut line = format!("{{ let w_ = {temp};");
        for k in 0..4 {
            if d.mask & (1 << k) != 0 {
                line += &format!(" {name}.{} = w_.{};", COMP[k] as char, COMP[k] as char);
            }
        }
        line += " }\n";
        self.body += &line;
    }

    fn write(&mut self, d: &Dst, v: &str) {
        let value = self.dest_value(d, v);
        self.commit(d, &value);
    }

    fn sample(&mut self, stage: u32, coord: &str, projected: bool, bias: bool) -> String {
        if self.p.pixel && self.p.major >= 2 && !self.p.samplers.contains_key(&stage) {
            self.fail(format!(
                "pixel shader 2.0 samples undeclared sampler s{stage}"
            ));
            return "vec4<f32>(0.0)".into();
        }
        if self.variant.unbound & (1u16 << stage) != 0 {
            return "vec4<f32>(0.0, 0.0, 0.0, 1.0)".into();
        }
        self.stages.insert(stage);
        let kind = self.stage_kind(stage);
        let c = if projected {
            format!("({coord} / ({coord}).w)")
        } else {
            coord.to_string()
        };
        if self.variant.depth & (1u16 << stage) != 0 {
            return format!("d9_shadow(tx{stage}, sm{stage}, {c})");
        }
        let lookup = if kind == S_CUBE || kind == S_VOLUME {
            format!("({c}).xyz")
        } else {
            format!("({c}).xy")
        };
        let sampled = if bias {
            format!("textureSampleBias(tx{stage}, sm{stage}, {lookup}, ({coord}).w)")
        } else {
            format!("textureSample(tx{stage}, sm{stage}, {lookup})")
        };
        if self.variant.alpha & (1u16 << stage) != 0 {
            format!("d9_alpha({sampled})")
        } else {
            sampled
        }
    }

    fn matrix_expr(&mut self, inst: &Inst) -> String {
        let cols = if inst.op == OP_M4X4 || inst.op == OP_M4X3 {
            4
        } else {
            3
        };
        let rows = match inst.op {
            OP_M4X4 | OP_M3X4 => 4,
            OP_M3X2 => 2,
            _ => 3,
        };
        let a = self.s(inst, 0);
        let mut e = Vec::new();
        for row in 0..4 {
            if row >= rows {
                e.push("0.0".into());
                continue;
            }
            let mut m = inst.src[1];
            m.index += row as u32;
            let line = self.src(&m);
            e.push(if cols == 4 {
                format!("dot({a}, {line})")
            } else {
                format!("dot(({a}).xyz, ({line}).xyz)")
            });
        }
        format!("vec4<f32>({})", e.join(", "))
    }

    fn pure(&mut self, inst: &Inst) -> Option<String> {
        let major = self.p.major;
        let minor = self.p.minor;
        let pixel = self.p.pixel;
        Some(match inst.op {
            OP_MOV | OP_MOVA => self.s(inst, 0),
            OP_ADD => format!("({} + {})", self.s(inst, 0), self.s(inst, 1)),
            OP_SUB => format!("({} - {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MUL => format!("({} * {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MAD => format!(
                "({} * {} + {})",
                self.s(inst, 0),
                self.s(inst, 1),
                self.s(inst, 2)
            ),
            OP_MIN => format!("min({}, {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MAX => format!("max({}, {})", self.s(inst, 0), self.s(inst, 1)),
            OP_SLT => format!(
                "select(vec4<f32>(0.0), vec4<f32>(1.0), {} < {})",
                self.s(inst, 0),
                self.s(inst, 1)
            ),
            OP_SGE => format!(
                "select(vec4<f32>(0.0), vec4<f32>(1.0), {} >= {})",
                self.s(inst, 0),
                self.s(inst, 1)
            ),
            OP_DP3 => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(dot(({a}).xyz, ({b}).xyz))")
            }
            OP_DP4 => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(dot({a}, {b}))")
            }
            OP_DP2ADD => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                format!("vec4<f32>(dot(({a}).xy, ({b}).xy) + ({c}).x)")
            }
            OP_DSX => format!("dpdx({})", self.s(inst, 0)),
            OP_DSY => format!("dpdy({})", self.s(inst, 0)),
            OP_RCP => format!("vec4<f32>(1.0 / ({}).x)", self.s(inst, 0)),
            OP_RSQ => format!("vec4<f32>(inverseSqrt(abs(({}).x)))", self.s(inst, 0)),
            OP_FRC => format!("fract({})", self.s(inst, 0)),
            OP_ABS => format!("abs({})", self.s(inst, 0)),
            OP_EXP => format!("vec4<f32>(exp2(({}).x))", self.s(inst, 0)),
            OP_EXPP => {
                let a = self.s(inst, 0);
                if major == 1 {
                    format!("vec4<f32>(exp2(floor(({a}).x)), fract(({a}).x), exp2(({a}).x), 1.0)")
                } else {
                    format!("vec4<f32>(exp2(({a}).x))")
                }
            }
            OP_LOG => format!("vec4<f32>(log2(abs(({}).x)))", self.s(inst, 0)),
            OP_LOGP => {
                let a = self.s(inst, 0);
                if major == 1 {
                    format!(
                        "vec4<f32>(floor(log2(abs(({a}).x))), abs(({a}).x) / exp2(floor(log2(abs(({a}).x)))), log2(abs(({a}).x)), 1.0)"
                    )
                } else {
                    format!("vec4<f32>(log2(abs(({a}).x)))")
                }
            }
            OP_POW => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(pow(abs(({a}).x), ({b}).x))")
            }
            OP_NRM => format!("d9_nrm({})", self.s(inst, 0)),
            OP_LRP => format!(
                "mix({}, {}, {})",
                self.s(inst, 2),
                self.s(inst, 1),
                self.s(inst, 0)
            ),
            OP_CND => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                if pixel && major == 1 && minor == 4 {
                    format!("select({c}, {b}, {a} > vec4<f32>(0.5))")
                } else {
                    format!("select({c}, {b}, ({a}).w > 0.5)")
                }
            }
            OP_CMP => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                format!("select({c}, {b}, {a} >= vec4<f32>(0.0))")
            }
            OP_LIT => format!("d9_lit({})", self.s(inst, 0)),
            OP_SINCOS => {
                let a = self.s(inst, 0);
                format!("vec4<f32>(cos(({a}).x), sin(({a}).x), 0.0, 0.0)")
            }
            OP_DST => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(1.0, ({a}).y * ({b}).y, ({a}).z, ({b}).w)")
            }
            OP_M4X4 | OP_M4X3 | OP_M3X4 | OP_M3X3 | OP_M3X2 => self.matrix_expr(inst),
            _ => return None,
        })
    }

    fn texcoord(&mut self, inst: &Inst) {
        if inst.nsrc > 0 {
            let a = self.s(inst, 0);
            self.write(&inst.dst, &format!("vec4<f32>(({a}).xyz, 1.0)"));
        } else {
            let n = inst.dst.index;
            self.write(
                &inst.dst,
                &format!("vec4<f32>(clamp(t[{n}].xyz, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0)"),
            );
        }
    }

    fn texkill(&mut self, inst: &Inst) {
        let s = inst.src[0];
        let v = if self.p.pixel && self.p.major == 1 && self.p.minor <= 3 && s.ty == R_ADDR {
            format!("tc[{}]", s.index)
        } else {
            self.base(&s)
        };
        if self.p.major >= 2 {
            self.body += &format!("if (any({v} < vec4<f32>(0.0))) {{ discard; }}\n");
        } else {
            self.body += &format!("if (any(({v}).xyz < vec3<f32>(0.0))) {{ discard; }}\n");
        }
    }

    fn texreg(&mut self, inst: &Inst, a: u32, b: u32) {
        let n = inst.dst.index;
        let src = self.s(inst, 0);
        let coord = format!(
            "vec4<f32>(({src}).{}, ({src}).{}, 0.0, 1.0)",
            COMP[a as usize] as char, COMP[b as usize] as char
        );
        let sampled = self.sample(n, &coord, false, false);
        self.write(&inst.dst, &sampled);
    }

    fn tex(&mut self, inst: &Inst) {
        if self.p.major >= 2 {
            let sampler = inst.src[1];
            if sampler.ty != R_SAMPLER || sampler.md != 0 || sampler.rel || sampler.swz != 0xe4 {
                self.fail("TEX sampler operand is not a canonical sampler register");
            }
            let stage = sampler.index;
            let coord = self.s(inst, 0);
            let sampled = self.sample(stage, &coord, inst.ctrl == 1, inst.ctrl == 2);
            self.write(&inst.dst, &sampled);
        } else if inst.nsrc > 0 {
            let n = inst.dst.index;
            let coord = self.s(inst, 0);
            let projected = self.variant.projected & (1u16 << n) != 0;
            let sampled = self.sample(n, &coord, projected, false);
            self.write(&inst.dst, &sampled);
        } else {
            let n = inst.dst.index;
            let coord = format!("t[{n}]");
            let projected = self.variant.projected & (1u16 << n) != 0;
            let sampled = self.sample(n, &coord, projected, false);
            self.write(&inst.dst, &sampled);
        }
    }

    fn instruction(&mut self, inst: &Inst) {
        match inst.op {
            OP_NOP => {}
            OP_TEXCOORD => self.texcoord(inst),
            OP_TEXKILL => self.texkill(inst),
            OP_TEX => self.tex(inst),
            OP_TEXREG2AR => self.texreg(inst, 3, 0),
            OP_TEXREG2GB => self.texreg(inst, 1, 2),
            _ => {
                if let Some(v) = self.pure(inst) {
                    self.write(&inst.dst, &v);
                } else {
                    self.fail(format!("instruction {} is not translated", inst.op));
                }
            }
        }
    }

    fn coissue_pair(&mut self, first: &Inst, second: &Inst) {
        let (Some(va), Some(vb)) = (self.pure(first), self.pure(second)) else {
            self.fail("coissue pair contains a non-arithmetic instruction");
            return;
        };
        let (da, db) = (first.dst, second.dst);
        let na = self.dst_name(&da);
        let nb = self.dst_name(&db);
        if na == nb && (da.mask & db.mask) != 0 {
            self.fail("coissue pair writes overlapping destination components");
            return;
        }
        let ta = self.dest_value(&da, &va);
        let tb = self.dest_value(&db, &vb);
        let n = self.seq;
        self.seq += 1;
        self.body += &format!("let pair{n}a = {ta};\nlet pair{n}b = {tb};\n");
        self.commit(&da, &format!("pair{n}a"));
        self.commit(&db, &format!("pair{n}b"));
    }

    fn comparison(&mut self, inst: &Inst) -> String {
        let a = self.s(inst, 0);
        let b = self.s(inst, 1);
        let op = match inst.ctrl {
            1 => ">",
            2 => "==",
            3 => ">=",
            4 => "<",
            5 => "!=",
            6 => "<=",
            _ => {
                self.fail(format!(
                    "comparison control {} is not implemented",
                    inst.ctrl
                ));
                "=="
            }
        };
        format!("((({a}).x) {op} (({b}).x))")
    }

    fn rep(&mut self, code: &[Inst], i: &mut usize, loops: u32) {
        let inst = code[*i];
        *i += 1;
        if inst.src[0].ty != R_CONSTINT {
            self.fail("REP count is not an integer constant");
            return;
        }
        let count = self.s(&inst, 0);
        let n = self.seq;
        self.seq += 1;
        self.body += &format!("let rep{n} = bitcast<u32>(i32(({count}).x));\n");
        self.body += &format!("for (var rep_i{n}: u32 = 0u; rep_i{n} < rep{n}; rep_i{n}++) {{\n");
        let stop = self.emit_sequence(code, i, &[OP_ENDREP], loops + 1);
        if stop == Some(OP_ENDREP) {
            *i += 1;
        } else {
            self.fail("REP has no matching ENDREP");
        }
        self.body += "}\n";
    }

    fn conditional(&mut self, code: &[Inst], i: &mut usize, loops: u32) {
        let inst = code[*i];
        *i += 1;
        let cond = if inst.op == OP_IFC {
            self.comparison(&inst)
        } else {
            let value = self.s(&inst, 0);
            format!("(({value}).x != 0.0)")
        };
        self.body += &format!("if ({cond}) {{\n");
        let stop = self.emit_sequence(code, i, &[OP_ELSE, OP_ENDIF], loops);
        match stop {
            Some(OP_ENDIF) => {
                *i += 1;
            }
            Some(OP_ELSE) => {
                *i += 1;
                self.body += "} else {\n";
                let stop = self.emit_sequence(code, i, &[OP_ENDIF], loops);
                if stop == Some(OP_ENDIF) {
                    *i += 1;
                } else {
                    self.fail("IF has no matching ENDIF");
                }
            }
            _ => self.fail("IF has no matching ENDIF"),
        }
        self.body += "}\n";
    }

    fn emit_sequence(
        &mut self,
        code: &[Inst],
        i: &mut usize,
        stops: &[u32],
        loops: u32,
    ) -> Option<u32> {
        while *i < code.len() {
            let op = code[*i].op;
            if stops.contains(&op) {
                return Some(op);
            }
            if code[*i].coissue {
                self.fail("coissue instruction without a preceding instruction");
                *i += 1;
                continue;
            }
            if *i + 1 < code.len() && code[*i + 1].coissue {
                self.coissue_pair(&code[*i], &code[*i + 1]);
                *i += 2;
            } else {
                match op {
                    OP_REP => self.rep(code, i, loops),
                    OP_IF | OP_IFC => self.conditional(code, i, loops),
                    OP_BREAK => {
                        if loops == 0 {
                            self.fail("BREAK is used outside a REP loop");
                        } else {
                            self.body += "break;\n";
                        }
                        *i += 1;
                    }
                    OP_BREAKC => {
                        let inst = code[*i];
                        *i += 1;
                        if loops == 0 {
                            self.fail("BREAKC is used outside a REP loop");
                        } else {
                            let cond = self.comparison(&inst);
                            self.body += &format!("if ({cond}) {{ break; }}\n");
                        }
                    }
                    OP_ENDREP | OP_ELSE | OP_ENDIF => {
                        self.fail(format!("unexpected control instruction {op}"));
                        *i += 1;
                    }
                    _ => {
                        let inst = code[*i];
                        self.instruction(&inst);
                        *i += 1;
                    }
                }
            }
            if self.failed.is_some() {
                break;
            }
        }
        None
    }

    fn generate(&mut self) -> Result<(), RenderError> {
        let code = self.p.code.clone();
        let mut i = 0usize;
        self.emit_sequence(&code, &mut i, &[], 0);
        if let Some(cause) = self.failed.take() {
            return Err(fail(cause));
        }
        Ok(())
    }

    fn constants(&self) -> String {
        let (binding, bank) = if self.p.pixel { (2, "pcb") } else { (0, "vcb") };
        let mut s = format!(
            "@group(0) @binding({binding}) var<uniform> {bank}: array<vec4<f32>, {}>;\n",
            self.const_limit()
        );
        for (index, value) in &self.p.defs {
            let b = value.map(f32::to_bits);
            s += &format!(
                "fn d9_c{index}() -> vec4<f32> {{ return bitcast<vec4<f32>>(vec4<u32>({}u, {}u, {}u, {}u)); }}\n",
                b[0], b[1], b[2], b[3]
            );
        }
        if self.p.relative && !self.p.defs.is_empty() {
            s += "fn d9_const(index: i32) -> vec4<f32> {\nswitch index {\n";
            for index in self.p.defs.keys() {
                s += &format!("case {index}: {{ return d9_c{index}(); }}\n");
            }
            s += &format!("default: {{ return {bank}[index]; }}\n}}\n}}\n");
        }
        s
    }

    fn compose_vertex(&mut self) -> String {
        let mut inputs = Vec::new();
        for d in &self.p.inputs {
            if d.ty == R_INPUT && !inputs.contains(&d.index) {
                inputs.push(d.index);
            }
        }
        let mut s = String::from("struct VIn {\n");
        if inputs.is_empty() {
            s += "    @builtin(vertex_index) vid: u32,\n";
        } else {
            for i in &inputs {
                let ty = if self.variant.raw_inputs & (1 << i) != 0 {
                    "vec4<u32>"
                } else {
                    "vec4<f32>"
                };
                s += &format!("    @location({i}) v{i}: {ty},\n");
            }
        }
        s += "}\n";
        s += &self.constants();
        s += "@group(0) @binding(1) var<uniform> vsp: VSP;\n";
        s += "@vertex\nfn vs_main(vin: VIn) -> V2F {\n";
        s += "    var r: array<vec4<f32>, 32>;\n";
        s += "    var a0 = vec4<f32>(0.0);\n";
        s += "    var opos = vec4<f32>(0.0);\n";
        s += "    var ofog = vec4<f32>(1.0);\n";
        s += "    var opsize = vec4<f32>(1.0);\n";
        s += "    var od = array<vec4<f32>, 2>(vec4<f32>(0.0), vec4<f32>(0.0));\n";
        if self.p.major >= 3 {
            s += "    var ov: array<vec4<f32>, 12>;\n";
        } else {
            s += "    var ot: array<vec4<f32>, 8>;\n";
        }
        s += "    var vv: array<vec4<f32>, 16>;\n";
        for i in &inputs {
            let value = if self.variant.raw_inputs & (1 << i) != 0 {
                format!("vec4<f32>(vin.v{i})")
            } else {
                format!("vin.v{i}")
            };
            s += &format!("    vv[{i}] = {value} * vsp.ascale[{i}];\n");
        }
        s += &self.body;
        s += "    var o: V2F;\n";
        if self.p.major >= 3 {
            let mut position = "opos".to_string();
            for d in &self.p.outputs {
                let value = format!("ov[{}]", d.index);
                match d.usage {
                    0 | 9 => position = value,
                    // DIVERGENCE(original): point size output is ignored; point sprites are rejected at draw time.
                    4 => {}
                    5 if d.usage_index < 8 => {
                        s += &format!("    o.t{} = {value};\n", d.usage_index);
                    }
                    10 if d.usage_index < 2 => {
                        s += &format!("    o.d{} = {value};\n", d.usage_index);
                    }
                    11 => s += &format!("    o.fog = {value}.x;\n"),
                    _ => self.fail(format!("vertex output usage {}", d.usage)),
                }
            }
            s += &format!(
                "    let d9_pos = vec4<f32>({position}.xy + vsp.halfpix.xy * {position}.w, {position}.zw);\n"
            );
            s += "    o.pos = d9_pos;\n";
            s += "    o.fogz = d9_pos.z;\n";
        } else {
            s += "    o.pos = vec4<f32>(opos.xy + vsp.halfpix.xy * opos.w, opos.zw);\n";
            s += "    o.d0 = od[0];\n    o.d1 = od[1];\n";
            for i in 0..8 {
                s += &format!("    o.t{i} = ot[{i}];\n");
            }
            s += "    o.fog = ofog.x;\n    o.fogz = opos.z;\n";
        }
        s += "    return o;\n}\n";
        s
    }

    fn compose_pixel(&mut self) -> String {
        let mut s = String::new();
        s += &self.constants();
        s += "@group(0) @binding(3) var<uniform> psp: PSP;\n";
        for &stage in &self.stages {
            let depth = self.variant.depth & (1u16 << stage) != 0;
            let kind = self.stage_kind(stage);
            let ty = if depth {
                "texture_depth_2d"
            } else if kind == S_CUBE {
                "texture_cube<f32>"
            } else if kind == S_VOLUME {
                "texture_3d<f32>"
            } else {
                "texture_2d<f32>"
            };
            let sampler = if depth {
                "sampler_comparison"
            } else {
                "sampler"
            };
            s += &format!("@group(0) @binding({}) var tx{stage}: {ty};\n", 4 + stage);
            s += &format!(
                "@group(0) @binding({}) var sm{stage}: {sampler};\n",
                20 + stage
            );
        }
        s += "@fragment\nfn ps_main(vin: V2F) -> @location(0) vec4<f32> {\n";
        s += "    var r: array<vec4<f32>, 32>;\n";
        s += "    var tc = array<vec4<f32>, 8>(vin.t0, vin.t1, vin.t2, vin.t3, vin.t4, vin.t5, vin.t6, vin.t7);\n";
        s += "    var t = tc;\n";
        if self.p.major >= 3 {
            s += "    var iv: array<vec4<f32>, 16>;\n";
            let inputs = self.p.inputs.clone();
            for d in &inputs {
                if d.ty != R_INPUT {
                    continue;
                }
                let value = match (d.usage, d.usage_index) {
                    (5, i) if i < 8 => format!("vin.t{i}"),
                    (10, i) if i < 2 => format!("vin.d{i}"),
                    (11, _) => "vec4<f32>(vin.fog, 0.0, 0.0, 0.0)".to_string(),
                    _ => {
                        self.fail(format!("pixel input usage {}", d.usage));
                        continue;
                    }
                };
                s += &format!("    iv[{}] = {value};\n", d.index);
            }
        } else {
            s += "    var iv = array<vec4<f32>, 2>(vin.d0, vin.d1);\n";
        }
        s += "    var oc: array<vec4<f32>, 4>;\n";
        s += &self.body;
        if self.p.major >= 2 {
            s += "    var col = oc[0];\n";
        } else {
            s += "    var col = clamp(r[0], vec4<f32>(0.0), vec4<f32>(1.0));\n";
        }
        s += "    return col;\n}\n";
        s
    }

    fn compose(&mut self) -> String {
        let mut s = String::from(PRELUDE);
        if self.p.pixel {
            s += &self.compose_pixel();
        } else {
            s += &self.compose_vertex();
        }
        s
    }
}

pub fn translate(p: &Program, variant: &Variant) -> Result<String, RenderError> {
    let mut g = Gen::new(p, variant);
    g.generate()?;
    let source = g.compose();
    if let Some(cause) = g.failed.take() {
        return Err(fail(cause));
    }
    Ok(source)
}

pub fn validate(source: &str) -> Result<(), RenderError> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| {
        fail(format!(
            "translated WGSL does not parse: {}",
            e.emit_to_string(source)
        ))
    })?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .map_err(|e| fail(format!("translated WGSL does not validate: {e:?}")))?;
    Ok(())
}
