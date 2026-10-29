//! Writes the generated tree: address-bucketed body chunks, entry-thunk chunks, `funcs.h`,
//! `body.h`, the dispatch `table.c`, `call_facts.c` and `symbols.json`, using the emitted-body
//! model from `rk-emit`. Callers stage into a fresh directory because stale chunks are not
//! removed.

pub mod reloc;
mod symbols;
mod table;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use rayon::prelude::*;
use rk_core::contract::DirectArguments;
use rk_core::program::Program;
use rk_core::settings::Settings;
use rk_core::{Addr, Error, Result};
use rk_emit::{Body, Declaration, Preludes};

pub use reloc as reloc_output;
pub use symbols::{symbol_rows, SymbolRow};

pub const CHUNK_ADDRESS_BITS: u32 = 16;
pub const CHUNK_SOURCE_BUDGET: usize = 4 * 1024 * 1024;
pub const ENTRY_ADDRESS_BITS: u32 = 16;

const BODY_HEADER: &str = r#"/* generated -- translated bodies do not depend on replacement policy */
#ifndef RECOMP_BODY_H
#define RECOMP_BODY_H
#include "x86.h"
#include "intrinsics.h"
#include "seh.h"
#define RECOMP_ENTRY_CAT_(a, b) a##b
#define RECOMP_ENTRY_CAT(a, b) RECOMP_ENTRY_CAT_(a, b)
#define CALL_FN(a) RECOMP_ENTRY_CAT(entry_, a)(c)
#define CALL_RAW(a) RECOMP_ENTRY_CAT(call_, a)(c)
#endif
"#;

const ENTRY_STUB_INCLUDES: &str = r#"#include "x86.h"
#ifdef RECOMP_OVERRIDE_HEADER
#include RECOMP_OVERRIDE_HEADER
#endif
"#;

const ENTRY_STUB_PRELUDE: &str = r#"void recomp_enter(X86 *c, uint32_t index);
void recomp_enter_raw(X86 *c, uint32_t index, void (*raw)(X86 *));
#if RECOMP_CALL_HOOKS
#define RECOMP_SLOW_(i) RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow[i], __ATOMIC_RELAXED))
#else
#define RECOMP_SLOW_(i) RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow_routes, __ATOMIC_RELAXED))
#endif
#define RECOMP_ENTER(i, base) \
    do { recomp_execution_checkpoint(c); if (RECOMP_SLOW_(i)) recomp_enter(c, i); else base; } while (0)
#define RECOMP_CALL(i, raw) \
    do { recomp_execution_checkpoint(c); if (RECOMP_SLOW_(i)) recomp_enter_raw(c, i, raw); else raw(c); } while (0)
"#;

const MODULE_ENTRY_STUB_PRELUDE: &str = r#"
extern uint8_t recomp_{key}_hooked[];
void recomp_enter_{key}(X86 *c, uint32_t index);
void recomp_hook_{key}(X86 *c, uint32_t index);
#if RECOMP_CALL_HOOKS
#define RECOMP_ENTER(i, base) \
    do { \
        if (RECOMP_UNLIKELY(recomp_profile_enabled || recomp_frame_watch)) \
            recomp_enter_{key}(c, i); \
        else { \
            recomp_execution_checkpoint(c); \
            if (RECOMP_UNLIKELY(__atomic_load_n(&recomp_{key}_hooked[i], __ATOMIC_ACQUIRE))) \
                recomp_hook_{key}(c, i); \
            else base; \
        } \
    } while (0)
#else
#define RECOMP_ENTER(i, base) \
    do { \
        if (RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow_routes, __ATOMIC_RELAXED))) \
            recomp_enter_{key}(c, i); \
        else { recomp_execution_checkpoint(c); base; } \
    } while (0)
#endif
#define RECOMP_CALL(i, raw) RECOMP_ENTER(i, raw(c))
"#;

const DIRECT_GUARD: &str = r#"static inline int recomp_direct_(X86 *c, uint32_t checkpoints, uint8_t ready)
{
    if (RECOMP_UNLIKELY(recomp_slow_routes || !ready || c->checkpoint_countdown <= checkpoints))
        return 0;
    --c->checkpoint_countdown;
    return 1;
}
"#;

pub struct Translation<'a> {
    pub settings: &'a Settings,
    pub program: &'a Program,
    pub bodies: Vec<Body>,
    pub preludes: Preludes,
    pub call_returns: Vec<Addr>,
    pub call_facts: BTreeMap<Addr, Vec<Addr>>,
    pub cw_effects: BTreeMap<Addr, (u16, u16, u16)>,
    pub direct_variants: BTreeSet<Addr>,
}

pub struct Written {
    pub chunks: usize,
    pub entry_chunks: usize,
    pub symbols: Vec<SymbolRow>,
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| Error::io(path, e))
}

pub fn write(out: &Path, t: &Translation) -> Result<Written> {
    std::fs::create_dir_all(out).map_err(|e| Error::io(out, e))?;
    let entry_names: Vec<Addr> = t
        .program
        .entries
        .keys()
        .filter(|addr| !t.program.region_entries.contains_key(addr))
        .copied()
        .collect();
    let symbols = symbol_rows(t.settings, t.program, &entry_names)?;
    let module = t.settings.module.as_ref().map(|m| m.key.as_str());
    let prefix = module.map_or("recomp_".to_string(), |key| format!("recomp_{key}_"));

    write_file(&out.join("funcs.h"), &funcs_header(&entry_names))?;
    let mut body_header = BODY_HEADER.to_string();
    if let Some(config) = &t.settings.dispatch {
        if config.shift <= 8 && !entry_names.is_empty() {
            let base = entry_names[0] >> config.shift << config.shift;
            let span =
                (((entry_names.last().unwrap() - base) as u64 >> config.shift) + 1) << config.shift;
            let constants = format!("#define RECOMP_DISPATCH_ENTRIES {prefix}dispatch_entries\n#define RECOMP_DISPATCH_BASE 0x{base:08x}u\n#define RECOMP_DISPATCH_SPAN {span}ull\n#define RECOMP_DISPATCH_SHIFT {}u\n#include \"x86.h\"", config.shift);
            body_header = body_header.replace("#include \"x86.h\"", &constants);
        }
    }
    write_file(&out.join("body.h"), &body_header)?;
    let chunks = partition_chunks(&t.bodies, CHUNK_SOURCE_BUDGET);
    let chunk_count = chunks.len();
    chunks
        .into_par_iter()
        .map(|(name, group)| write_file(&out.join(&name), &body_chunk(t, &group, module)))
        .collect::<Result<()>>()?;
    let entry_chunks = entry_chunks(&entry_names, module, t);
    let entry_chunk_count = entry_chunks.len();
    for (name, text) in entry_chunks {
        write_file(&out.join(name), &text)?;
    }
    let table = match &t.settings.module {
        Some(m) => table::module_table(
            &entry_names,
            &symbols,
            &t.call_returns,
            m,
            t.program,
            t.settings.dispatch.as_ref(),
        )?,
        None => table::main_table(
            &entry_names,
            &symbols,
            &t.call_returns,
            t.program,
            t.settings.dispatch.as_ref(),
        )?,
    };
    write_file(&out.join("table.c"), &table)?;
    write_file(
        &out.join("call_facts.c"),
        &table::call_facts(&t.call_facts, &t.cw_effects, &prefix),
    )?;
    write_file(
        &out.join("symbols.json"),
        &symbols::symbols_json(t.settings, t.program, &symbols)?,
    )?;
    Ok(Written {
        chunks: chunk_count,
        entry_chunks: entry_chunk_count,
        symbols,
    })
}

fn partition_chunks(bodies: &[Body], budget: usize) -> Vec<(String, Vec<&Body>)> {
    let weight = |b: &Body| b.text.len() + usize::from(!b.text.is_empty());
    let mut result = Vec::new();
    let mut by_address: BTreeMap<Addr, Vec<&Body>> = BTreeMap::new();
    for body in bodies {
        by_address.entry(body.addr).or_default().push(body);
    }
    let mut buckets: BTreeMap<Addr, Vec<Vec<&Body>>> = BTreeMap::new();
    for (addr, group) in by_address {
        if group.iter().map(|body| weight(body)).sum::<usize>() >= budget {
            result.push((format!("chunk_fn_{addr:08x}.c"), group));
        } else {
            buckets
                .entry(addr >> CHUNK_ADDRESS_BITS)
                .or_default()
                .push(group);
        }
    }
    fn split<'a>(
        group: Vec<Vec<&'a Body>>,
        base: Addr,
        bits: u32,
        budget: usize,
        weight: &dyn Fn(&Body) -> usize,
        result: &mut Vec<(String, Vec<&'a Body>)>,
    ) {
        let total = group
            .iter()
            .flatten()
            .map(|body| weight(body))
            .sum::<usize>();
        if group.len() == 1 || total <= budget || bits == 0 {
            result.push((
                format!("chunk_{base:08x}_{bits:02}.c"),
                group.into_iter().flatten().collect(),
            ));
            return;
        }
        let half = base + (1 << (bits - 1));
        let (low, high): (Vec<_>, Vec<_>) = group.into_iter().partition(|b| b[0].addr < half);
        for (children, start) in [(low, base), (high, half)] {
            if !children.is_empty() {
                split(children, start, bits - 1, budget, weight, result);
            }
        }
    }
    for (bucket, group) in buckets {
        split(
            group,
            bucket << CHUNK_ADDRESS_BITS,
            CHUNK_ADDRESS_BITS,
            budget,
            &weight,
            &mut result,
        );
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn body_chunk(t: &Translation, group: &[&Body], module: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("/* generated by recomp-translate -- do not edit */\n#include \"body.h\"\n");
    if group.iter().any(|b| b.requires.seh_call) {
        s.push_str(
            "static void recomp_seh_frame_call_(X86 *c, void (*next)(X86 *), int adopt) {\n\
jmp_buf *b = adopt ? recomp_seh_frame_adopt(c) : recomp_seh_frame_enter(c);\n\
if (b && RECOMP_SETJMP(*b)) { recomp_seh_land(c); return; }\n\
next(c);\n}\n",
        );
    }
    if group.iter().any(|b| b.requires.arena) {
        s.push_str(&t.preludes.memory_helpers);
    }
    if group.iter().any(|b| b.requires.return_fast) {
        let prefix = module.map_or("recomp_".to_string(), |key| format!("recomp_{key}_"));
        s.push_str(
            &t.preludes
                .table_return_helpers
                .replace("recomp_call_return_", &format!("{prefix}call_return_")),
        );
    }
    if group.iter().any(|b| b.requires.fcom) {
        s.push_str(&t.preludes.float_helpers);
    }
    let declarations: BTreeSet<&Declaration> = group.iter().flat_map(|b| &b.declarations).collect();
    let mut raw = BTreeSet::new();
    let mut regions = BTreeSet::new();
    let mut direct = BTreeSet::new();
    let mut variants = BTreeSet::new();
    for declaration in declarations {
        match declaration {
            Declaration::Raw(addr) => {
                raw.insert(*addr);
            }
            Declaration::Region(addr) => {
                regions.insert(*addr);
            }
            Declaration::Direct(addr) => {
                direct.insert(*addr);
            }
            Declaration::DirectVariant {
                addr,
                input_arguments,
            } => {
                variants.insert((*addr, *input_arguments));
            }
        }
    }
    for addr in raw {
        let _ = writeln!(s, "void call_{addr:08x}(X86 *c);");
    }
    for addr in regions {
        let _ = writeln!(s, "void region_{addr:08x}(X86 *c, uint32_t target);");
    }
    if !direct.is_empty() {
        s.push_str(DIRECT_GUARD);
    }
    for addr in direct {
        let _ = writeln!(
            s,
            "void raw_{addr:08x}(X86 *c);\nextern const uint8_t call_ready_{addr:08x};"
        );
    }
    for (addr, input_arguments) in variants {
        let parameter = argument_parameters(input_arguments);
        let _ = writeln!(s, "void direct_{addr:08x}(X86 *c{parameter});");
    }
    s.push('\n');
    for body in group {
        let _ = write!(
            s,
            "/* FUN_{:08x}  {} insns  {:08x}..{:08x}",
            body.addr, body.insns, body.first, body.last
        );
        if let Some(entries) = t.program.interior_by_function.get(&body.addr) {
            s.push_str("  entries:");
            for entry in entries {
                let _ = write!(s, " {entry:08x}");
            }
        }
        s.push_str(" */\n");
        s.push_str(&body.text);
        s.push_str("\n\n");
    }
    s
}

fn argument_parameters(arguments: DirectArguments) -> String {
    rk_emit::argument_parameters(arguments)
        .into_iter()
        .map(|(ty, name)| format!(", {ty} {name}"))
        .collect()
}

fn entry_chunks(
    entry_names: &[Addr],
    module: Option<&str>,
    t: &Translation,
) -> Vec<(String, String)> {
    let mut raw = BTreeSet::new();
    let mut direct = BTreeSet::new();
    for declaration in t.bodies.iter().flat_map(|body| &body.declarations) {
        match *declaration {
            Declaration::Raw(target) => {
                raw.insert(target);
            }
            Declaration::Direct(target) => {
                direct.insert(target);
            }
            Declaration::Region(_) | Declaration::DirectVariant { .. } => {}
        }
    }
    let mut groups: BTreeMap<Addr, Vec<(usize, Addr)>> = BTreeMap::new();
    for (index, &addr) in entry_names.iter().enumerate() {
        groups
            .entry(addr >> ENTRY_ADDRESS_BITS)
            .or_default()
            .push((index, addr));
    }
    groups
        .into_iter()
        .map(|(bucket, entries)| {
            let mut s = String::from("/* generated stable entry thunks -- do not edit */\n");
            s.push_str(ENTRY_STUB_INCLUDES);
            match module {
                Some(key) => s.push_str(&MODULE_ENTRY_STUB_PRELUDE.replace("{key}", key)),
                None => s.push_str(ENTRY_STUB_PRELUDE),
            }
            for (index, addr) in &entries {
                let _ = write!(
                    s,
                    "#ifndef FN_{addr:08x}\n#define FN_{addr:08x} fn_{addr:08x}\n#define RECOMP_RAW_{addr:08x}\n#endif\n\
                     void FN_{addr:08x}(X86 *c);\n\
                     void entry_{addr:08x}(X86 *c) {{\n#ifndef RECOMP_RAW_{addr:08x}\n    x86_cc_settle(c);\n#endif\n    RECOMP_ENTER({index}u, FN_{addr:08x}(c));\n}}\n"
                );
            }
            for (index, addr) in &entries {
                if raw.contains(addr) {
                    let _ = write!(
                        s,
                        "#ifdef RECOMP_RAW_{addr:08x}\nvoid raw_{addr:08x}(X86 *c);\n#endif\n\
                         void call_{addr:08x}(X86 *c) {{\n#ifdef RECOMP_RAW_{addr:08x}\n    RECOMP_CALL({index}u, raw_{addr:08x});\n#else\n    x86_cc_settle(c);\n    RECOMP_ENTER({index}u, FN_{addr:08x}(c));\n#endif\n}}\n"
                    );
                }
                if direct.contains(addr) {
                    let _ = write!(
                        s,
                        "#ifdef RECOMP_RAW_{addr:08x}\nconst uint8_t call_ready_{addr:08x} = 1;\n#else\nconst uint8_t call_ready_{addr:08x} = 0;\n#endif\n"
                    );
                }

            }
            (format!("chunk_entries_{:08x}.c", bucket << ENTRY_ADDRESS_BITS), s)
        })
        .collect()
}

fn funcs_header(entry_names: &[Addr]) -> String {
    let mut s = String::with_capacity(entry_names.len() * 160);
    s.push_str("/* generated by recomp-translate -- do not edit */\n");
    s.push_str("#ifndef RECOMP_FUNCS_H\n#define RECOMP_FUNCS_H\n");
    s.push_str("#include \"x86.h\"\n#include \"intrinsics.h\"\n#include \"seh.h\"\n");
    s.push_str("/* Define FN_<addr> in RECOMP_OVERRIDE_HEADER to select a native\n * base in table.c. Stable entries route direct and computed calls\n * through that selection without recompiling their callers. */\n");
    s.push_str("#ifdef RECOMP_OVERRIDE_HEADER\n#include RECOMP_OVERRIDE_HEADER\n#endif\n");
    s.push_str("#define FN_CAT_(a, b) a##b\n#define FN_CAT(a, b) FN_CAT_(a, b)\n");
    s.push_str("#define FN(a) FN_CAT(FN_, a)\n#define FIDX(a) FN_CAT(FIDX_, a)\n");
    s.push_str("#include \"body.h\"\n");
    for (i, a) in entry_names.iter().enumerate() {
        let _ = writeln!(s, "#define FIDX_{a:08x} {i}u");
    }
    for a in entry_names {
        let _ = write!(
            s,
            "#ifndef FN_{a:08x}\n#define FN_{a:08x} fn_{a:08x}\n#endif\n"
        );
    }
    for a in entry_names {
        let _ = write!(
            s,
            "void fn_{a:08x}(X86 *c);\nvoid FN({a:08x})(X86 *c);\nvoid entry_{a:08x}(X86 *c);\n"
        );
    }
    s.push_str("#endif\n");
    s
}
