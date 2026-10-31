use std::fmt::Write as _;
use std::path::Path;

use rk_core::{Addr, Error, Result};

use crate::{body_chunk, write_file, Translation, BODY_HEADER};

pub fn write(out: &Path, t: &Translation, ranges: &[(Addr, u32)]) -> Result<()> {
    std::fs::create_dir_all(out).map_err(|e| Error::io(out, e))?;
    let resume_safe = !t.bodies.iter().any(|body| body.requires.seh_call);
    let header = BODY_HEADER
        .replace(
            "RECOMP_ENTRY_CAT(entry_, a)(c)",
            "recomp_call(c, RECOMP_ENTRY_CAT(0x, a))",
        )
        .replace(
            "RECOMP_ENTRY_CAT(call_, a)(c)",
            "recomp_call(c, RECOMP_ENTRY_CAT(0x, a))",
        );
    let header = header.replace(
        "#endif\n",
        r#"
static inline void jit_guest_call_(X86 *c, uint32_t target) {
    unsigned depth = recomp_dynamic_return_push(c, rd32(c->r[R_ESP]));
    recomp_call(c, target);
    recomp_dynamic_return_pop(c, depth);
}
static inline void jit_guest_icall_(X86 *c, uint32_t target, uint32_t site) {
    unsigned depth = recomp_dynamic_return_push(c, rd32(c->r[R_ESP]));
    recomp_icall(c, target, site);
    recomp_dynamic_return_pop(c, depth);
}
#define recomp_call(c, target) do { \
    uint32_t jit_return_ = rd32((c)->r[R_ESP]); \
    jit_guest_call_(c, target); \
    if ((c)->eip != jit_return_) return; \
} while (0)
#define recomp_icall(c, target, site) do { \
    uint32_t jit_return_ = rd32((c)->r[R_ESP]); \
    jit_guest_icall_(c, target, site); \
    if ((c)->eip != jit_return_) return; \
} while (0)
#define recomp_icall_guest(c, target, site) recomp_icall(c, target, site)
#endif
"#,
    );
    let header = if resume_safe {
        header.replace("#endif\n", "#include \"jit.h\"\n#define recomp_jump(c, target) recomp_jit_jump(c, target)\n#define recomp_region_jump(c, fn, target) recomp_jit_jump(c, target)\n#endif\n")
    } else {
        header
    };
    write_file(&out.join("body.h"), &header)?;
    let bodies: Vec<_> = t.bodies.iter().collect();
    let mut body = body_chunk(t, &bodies, None).replace("recomp_call_return_", "jit_call_return_");
    if resume_safe {
        body = body
            .lines()
            .map(|line| {
                if let Some((prefix, tail)) = line.split_once("CALL_RAW(") {
                    if let Some((address, suffix)) = tail.split_once("); return;") {
                        return format!("{prefix}recomp_jump(c, 0x{address}u); return;{suffix}");
                    }
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        body.push('\n');
    }
    write_file(&out.join("batch.c"), &body)?;
    let mut table = String::from("#include \"jit.h\"\n");
    for addr in t.program.entries.keys() {
        if let Some(owner) = t.program.region_entries.get(addr) {
            let _ = writeln!(table, "void region_{owner:08x}(X86 *, uint32_t);\nstatic void jit_entry_{addr:08x}(X86 *c) {{ region_{owner:08x}(c, 0x{addr:08x}u); }}");
        } else {
            let _ = writeln!(table, "void fn_{addr:08x}(X86 *);\nstatic void jit_entry_{addr:08x}(X86 *c) {{ fn_{addr:08x}(c); }}");
        }
    }
    table.push_str("static const RecompDispatchEntry entries[] = {\n");
    for addr in t.program.entries.keys() {
        let flags = if resume_safe {
            "RECOMP_JIT_RESUME_SAFE"
        } else {
            "0"
        };
        let _ = writeln!(table, "{{0x{addr:08x}u, {flags}, jit_entry_{addr:08x}}},");
    }
    table.push_str("};\n");
    for (i, &(addr, size)) in ranges.iter().enumerate() {
        let _ = writeln!(table, "static const uint8_t bytes_{i}[] = {{");
        for byte in t.program.image.bytes_at(addr, size as usize)? {
            let _ = write!(table, "{byte},");
        }
        table.push_str("};\n");
    }
    table.push_str("static const RecompJitBytes ranges[] = {\n");
    for (i, &(addr, size)) in ranges.iter().enumerate() {
        let _ = writeln!(table, "{{0x{addr:08x}u, {size}u, bytes_{i}}},");
    }
    table.push_str("};\nstatic const uint32_t returns[] = {0,\n");
    for addr in &t.call_returns {
        let _ = writeln!(table, "0x{addr:08x}u,");
    }
    table.push_str("};\n");
    super::table::call_return_bitmap(&mut table, &t.call_returns, "jit_");
    let _ = writeln!(table, "static const RecompJitModule module = {{1, sizeof(X86), 0x{:08x}u, \"{}\", {}, {}, {}, entries, ranges, returns + 1}};", t.settings.base, t.settings.sha256, t.program.entries.len(), ranges.len(), t.call_returns.len());
    table.push_str("__attribute__((visibility(\"default\"))) const RecompJitModule *recomp_jit_descriptor(void) { return &module; }\n");
    write_file(&out.join("module.c"), &table)
}
