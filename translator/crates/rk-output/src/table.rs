use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rk_core::program::Program;
use rk_core::settings::{Dispatch, Module};
use rk_core::{Addr, Error, Result};

use crate::SymbolRow;

const TABLE_TAIL: &str = include_str!("../templates/table_tail.c");
const ENTRY_DISPATCH: &str = include_str!("../templates/entry_dispatch.c");

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn call_return_bitmap(s: &mut String, call_returns: &[Addr], prefix: &str) {
    let (first, words) = match (call_returns.first(), call_returns.last()) {
        (Some(&first), Some(&last)) => {
            let mut words = vec![0u32; (((last - first) >> 5) + 1) as usize];
            for &addr in call_returns {
                let offset = addr - first;
                words[(offset >> 5) as usize] |= 1 << (offset & 31);
            }
            (first, words)
        }
        _ => (0, vec![0]),
    };
    let _ = writeln!(
        s,
        "const uint32_t {prefix}call_return_min = 0x{first:08x}u;"
    );
    let _ = writeln!(
        s,
        "const uint32_t {prefix}call_return_span = {}u;",
        words.len() * 32
    );
    let _ = writeln!(s, "const uint32_t {prefix}call_return_bits[] = {{");
    for word in words {
        let _ = writeln!(s, "    0x{word:08x}u,");
    }
    s.push_str("};\n");
}

fn entry_dispatch(signature: &str, base_call: &str) -> String {
    ENTRY_DISPATCH
        .replace("{signature}", signature)
        .replace("{base_call}", base_call)
}

fn addr_rows(s: &mut String, addrs: &[Addr]) {
    for a in addrs {
        let _ = writeln!(s, "    0x{a:08x}u,");
    }
}

fn region_table(s: &mut String, entry_names: &[Addr], program: &Program, prefix: &str) -> usize {
    let owners: BTreeSet<Addr> = program.region_entries.values().copied().collect();
    let mut regions = Vec::new();
    for owner in owners {
        let _ = writeln!(s, "void region_{owner:08x}(X86 *, uint32_t);");
        let index = entry_names.binary_search(&owner).unwrap();
        for span in &program.functions[&owner].spans {
            regions.push((span.start, span.end(), owner, index));
        }
    }
    regions.sort_unstable();
    let mut bits = Vec::new();
    let mut offsets = Vec::new();
    for &(start, end, _, _) in &regions {
        offsets.push(bits.len());
        let mut bitmap = vec![0u8; ((end - start) as usize + 7) / 8];
        for (&entry, _) in program.entries.range(start..end) {
            let offset = (entry - start) as usize;
            bitmap[offset >> 3] |= 1u8 << (offset & 7);
        }
        bits.extend(bitmap);
    }
    let _ = writeln!(s, "static const uint8_t {prefix}region_boundaries[] = {{");
    for group in bits.chunks(32) {
        for byte in group {
            let _ = write!(s, "0x{byte:02x},");
        }
        s.push('\n');
    }
    if bits.is_empty() {
        s.push_str("0,\n");
    }
    let _ = writeln!(s, "}};\nstatic const RecompRegion {prefix}regions[] = {{");
    for (&(start, end, owner, index), offset) in regions.iter().zip(offsets) {
        let _ = writeln!(s, "    {{0x{start:08x}u, 0x{end:08x}u, {index}u, region_{owner:08x}, {prefix}region_boundaries + {offset}u}},");
    }
    if regions.is_empty() {
        s.push_str("    {0, 0, 0, 0, 0},\n");
    }
    let _ = writeln!(
        s,
        "}};\nstatic const uint32_t {prefix}region_count = {};",
        regions.len()
    );
    regions.len()
}

fn dispatch_table(
    s: &mut String,
    addrs: &[Addr],
    config: Option<&Dispatch>,
    prefix: &str,
    local: bool,
) -> Result<()> {
    let mut slots = BTreeMap::new();
    let (base, span, shift, max_probe, count) = if let Some(config) = config {
        if config.shift > 8 || config.max_probe > 256 {
            return Err(Error::input(
                "dispatch shift must be <= 8 and max_probe <= 256",
            ));
        }
        let shift = config.shift;
        let base = addrs.first().copied().unwrap_or(0) >> shift << shift;
        let span = addrs
            .last()
            .map_or(0u64, |&a| ((a - base) as u64 >> shift) + 1);
        let count = (span + config.max_probe as u64).max(1);
        if count > 16 * 1024 * 1024 {
            return Err(Error::input(
                "dispatch table exceeds 16 million slots; increase dispatch.shift",
            ));
        }
        for (index, &addr) in addrs.iter().enumerate() {
            let home = ((addr - base) >> shift) as usize;
            let mut slot = home;
            while slots.contains_key(&slot) {
                slot += 1;
            }
            if slot - home > config.max_probe as usize {
                return Err(Error::input(format!("dispatch target {addr:08x} needs displacement {}, exceeding configured max_probe {}", slot - home, config.max_probe)));
            }
            slots.insert(slot, (addr, index));
        }
        (base, span << shift, shift, config.max_probe, count as usize)
    } else {
        for (index, &addr) in addrs.iter().enumerate() {
            slots.insert(index, (addr, index));
        }
        (0, 0, 0, 0, addrs.len().max(1))
    };
    if config.is_some() {
        let _ = writeln!(s, "RecompDispatchEntry {prefix}dispatch_entries[{count}];");
        let _ = writeln!(
            s,
            "static const RecompDispatchEntry {prefix}dispatch_seeds[] = {{"
        );
        for (index, &addr) in addrs.iter().enumerate() {
            if index >= 0x80000000usize {
                return Err(Error::input("dispatch index exceeds 31 bits"));
            }
            let _ = writeln!(s, "    {{0x{addr:08x}u, {index}u, entry_{addr:08x}}},");
        }
    } else {
        let _ = writeln!(
            s,
            "static const RecompDispatchEntry {prefix}dispatch_entries[{count}] = {{"
        );
        for (&slot, &(addr, index)) in &slots {
            let _ = writeln!(
                s,
                "    [{slot}] = {{0x{addr:08x}u, {index}u, entry_{addr:08x}}},"
            );
        }
    }
    if addrs.is_empty() {
        s.push_str("    {0, 0, 0},\n");
    }
    let storage = if local { "static " } else { "" };
    let _ = writeln!(s, "}};\n{storage}const RecompDispatchTable {prefix}dispatch = {{ {prefix}dispatch_entries, 0x{base:08x}u, {span}ull, {shift}u, {max_probe}u, {count}u }};");
    if config.is_some() {
        let constructor = if local {
            ""
        } else {
            "__attribute__((constructor)) "
        };
        let _ = writeln!(s, "{constructor}static void {prefix}dispatch_startup(void)\n{{\n    recomp_dispatch_init({prefix}dispatch_entries, &{prefix}dispatch, {prefix}dispatch_seeds, {}u);\n}}", addrs.len());
    }
    Ok(())
}

pub fn main_table(
    entry_names: &[Addr],
    symbols: &[SymbolRow],
    call_returns: &[Addr],
    program: &Program,
    dispatch: Option<&Dispatch>,
) -> Result<String> {
    let mut s = String::with_capacity(entry_names.len() * 120);
    s.push_str("/* generated by recomp-translate -- do not edit */\n");
    s.push_str(
        "#include <stdio.h>\n#include <stdlib.h>\n#include \"funcs.h\"\n#include \"thunks.h\"\n",
    );
    s.push_str("#include \"discovery.h\"\n#include \"interp.h\"\n\n");
    s.push_str("const uint32_t recomp_func_addrs[] = {\n");
    addr_rows(&mut s, entry_names);
    s.push_str("};\n");
    let _ = writeln!(
        s,
        "const uint32_t recomp_func_count = {};\n",
        entry_names.len()
    );
    s.push_str("static const uint32_t recomp_call_returns[] = {\n");
    addr_rows(
        &mut s,
        if call_returns.is_empty() {
            &[0]
        } else {
            call_returns
        },
    );
    let _ = writeln!(
        s,
        "}};\nstatic const uint32_t recomp_call_return_count = {};\n",
        call_returns.len()
    );
    region_table(&mut s, entry_names, program, "recomp_");
    s.push_str("static const char *const profile_names[] = {\n");
    for row in symbols {
        let _ = writeln!(s, "    {},", json_string(&row.name));
    }
    s.push_str("};\n");
    s.push_str("void (*const recomp_base_ptrs[])(X86 *) = {\n");
    for a in entry_names {
        let _ = writeln!(s, "    FN({a:08x}),");
    }
    s.push_str("};\n\n");
    s.push_str(
        "static void recomp_run_base(X86 *c, uint32_t i)\n{\n    recomp_base_ptrs[i](c);\n}\n\n",
    );
    s.push_str("void (*const recomp_raw_ptrs[])(X86 *) = {\n");
    for a in entry_names {
        let _ = writeln!(s, "    fn_{a:08x},");
    }
    s.push_str("};\n\n");
    s.push_str("RecompHookFn recomp_hook_ptrs[] = {\n");
    for _ in entry_names {
        s.push_str("    recomp_run_base,\n");
    }
    s.push_str("};\n");
    let _ = writeln!(s, "uint8_t recomp_hooked[{}] = {{0}};", entry_names.len());
    let _ = writeln!(s, "uint8_t recomp_slow[{}] = {{0}};\n", entry_names.len());
    s.push_str(&entry_dispatch(
        "void recomp_enter(X86 *c, uint32_t i)",
        "        recomp_base_ptrs[i](c);",
    ));
    s.push_str(&entry_dispatch(
        "void recomp_enter_raw(X86 *c, uint32_t i, void (*raw)(X86 *))",
        "        raw(c);",
    ));
    call_return_bitmap(&mut s, call_returns, "recomp_");
    dispatch_table(&mut s, entry_names, dispatch, "recomp_", false)?;
    s.push_str(TABLE_TAIL);
    Ok(s)
}

pub fn module_table(
    entry_names: &[Addr],
    symbols: &[SymbolRow],
    call_returns: &[Addr],
    module: &Module,
    program: &Program,
    dispatch: Option<&Dispatch>,
) -> Result<String> {
    let key = &module.key;
    let p = format!("recomp_{key}_");
    let n = entry_names.len();
    let mut s = String::new();
    s.push_str("#include \"funcs.h\"\n#include \"thunks.h\"\n\n");
    let _ = writeln!(s, "static const uint32_t {p}addrs[] = {{");
    addr_rows(&mut s, entry_names);
    let _ = writeln!(s, "}};\nstatic const uint32_t {p}returns[] = {{");
    addr_rows(
        &mut s,
        if call_returns.is_empty() {
            &[0]
        } else {
            call_returns
        },
    );
    s.push_str("};\n");
    call_return_bitmap(&mut s, call_returns, &p);
    let _ = writeln!(s, "static const char *const {p}names[] = {{");
    for row in symbols {
        let _ = writeln!(s, "    {},", json_string(&row.name));
    }
    let _ = writeln!(s, "}};\nstatic void (*const {p}base[])(X86 *) = {{");
    for a in entry_names {
        let _ = writeln!(s, "    FN({a:08x}),");
    }
    s.push_str("};\n");
    let _ = writeln!(
        s,
        "static void {p}run(X86 *c, uint32_t i) {{ {p}base[i](c); }}"
    );
    let _ = writeln!(s, "static RecompHookFn {p}hooks[] = {{");
    for _ in entry_names {
        let _ = writeln!(s, "    {p}run,");
    }
    let _ = writeln!(s, "}};\nuint8_t {p}hooked[{n}];");
    let _ = write!(s, "void recomp_hook_{key}(X86 *c, uint32_t i)\n{{\n");
    let _ = write!(s, "    x86_cc_settle(c); {p}hooks[i](c, i);\n}}\n");
    let _ = write!(s, "void recomp_enter_{key}(X86 *c, uint32_t i)\n{{\n");
    s.push_str("    if (!recomp_profile_enabled && !recomp_frame_watch) {\n");
    s.push_str("        recomp_execution_checkpoint(c);\n");
    let _ = writeln!(
        s,
        "        if (__atomic_load_n(&{p}hooked[i], __ATOMIC_ACQUIRE)) recomp_hook_{key}(c, i);"
    );
    let _ = write!(s, "        else {p}base[i](c);\n        return;\n    }}\n");
    s.push_str("    RecompSaved saved_; recomp_save(c, &saved_);\n");
    s.push_str("    recomp_execution_checkpoint(c);\n");
    s.push_str("    if (recomp_profile_enabled) recomp_profile_push(i);\n");
    let _ = writeln!(
        s,
        "    if (__atomic_load_n(&{p}hooked[i], __ATOMIC_ACQUIRE)) recomp_hook_{key}(c, i);"
    );
    let _ = writeln!(s, "    else {p}base[i](c);");
    s.push_str("    if (recomp_profile_enabled) recomp_profile_pop();\n");
    let _ = write!(
        s,
        "    if (recomp_frame_watch) recomp_check_saved(c, {p}addrs[i], &saved_);\n}}\n"
    );
    let region_count = region_table(&mut s, entry_names, program, &p);
    dispatch_table(&mut s, entry_names, dispatch, &p, true)?;
    let _ = writeln!(s, "static const RecompModule {p}module = {{");
    let _ = writeln!(
        s,
        "    {}, 0x{:08x}u, 0x{:08x}u,",
        json_string(&module.name),
        module.base,
        module.base + module.size
    );
    let _ = writeln!(s, "    {p}addrs, {n}, {p}base, {p}hooks, {p}hooked,");
    let _ = writeln!(
        s,
        "    {p}returns, {}, {p}names, {p}regions, {region_count}u, &{p}dispatch\n}};",
        call_returns.len()
    );
    let _ = writeln!(
        s,
        "__attribute__((constructor)) static void {p}register(void)"
    );
    s.push_str("{\n");
    if dispatch.is_some() {
        let _ = writeln!(s, "    {p}dispatch_startup();");
    }
    let _ = writeln!(s, "    recomp_module_register(&{p}module);\n}}");
    Ok(s)
}

pub fn call_facts(
    facts: &BTreeMap<Addr, Vec<Addr>>,
    cw_effects: &BTreeMap<Addr, (u16, u16, u16)>,
    prefix: &str,
) -> String {
    let mut s = String::new();
    s.push_str("/* generated by recomp-translate -- do not edit */\n#include \"x86.h\"\n\n");
    let _ = writeln!(s, "const RecompCallFact {prefix}call_facts[] = {{");
    let mut targets = Vec::new();
    let mut offsets = BTreeMap::new();
    for (ret, found) in facts {
        let first = *offsets.entry(found.clone()).or_insert_with(|| {
            let first = targets.len();
            targets.extend_from_slice(found);
            first
        });
        let _ = writeln!(s, "    {{0x{ret:08x}u, {first}u, {}u}},", found.len());
    }
    if facts.is_empty() {
        s.push_str("    {0u, 0u, 0u},\n");
    }
    let _ = writeln!(
        s,
        "}};\nconst uint32_t {prefix}call_fact_count = {};\n",
        facts.len()
    );
    let _ = writeln!(s, "const uint32_t {prefix}call_fact_targets[] = {{");
    addr_rows(&mut s, if targets.is_empty() { &[0] } else { &targets });
    let _ = writeln!(s, "}};\n\nconst RecompCwEffect {prefix}cw_effects[] = {{");
    for (ret, (preserved, known, value)) in cw_effects {
        let _ = writeln!(
            s,
            "    {{0x{ret:08x}u, 0x{preserved:04x}u, 0x{known:04x}u, 0x{value:04x}u}},"
        );
    }
    if cw_effects.is_empty() {
        s.push_str("    {0x00000000u, 0x0000u, 0x0000u, 0x0000u},\n");
    }
    let _ = writeln!(
        s,
        "}};\nconst uint32_t {prefix}cw_effect_count = {};",
        cw_effects.len()
    );
    s
}
