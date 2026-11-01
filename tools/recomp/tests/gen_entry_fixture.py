"""Emit a game-free dispatch fixture using the Rust emitter's shared templates."""
from pathlib import Path
import sys


def write(out):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parents[3]
    templates = root / 'translator/crates/rk-output/templates'

    def template(name):
        return (templates / name).read_text()

    addresses = [0x0D001000, 0x0D001010, 0x0D001020, 0x0D001030]
    (out / 'body.h').write_text(template('body_header.h'))
    declarations = ''.join('void entry_%08x(X86 *c);\n' % addr for addr in addresses)
    (out / 'fixture_body.c').write_text('#include "body.h"\n' + declarations + '''
void fn_0d001000(X86 *c) { c->r[0] += 1; }
void fn_0d001010(X86 *c) { CALL_FN(0d001000); c->r[1]++; }
void fn_0d001020(X86 *c) { CALL_FN(0d001000); return; }
void fn_0d001030(X86 *c) { CALL_FN(0d001010); }
''')
    entries = '#undef RECOMP_CALL_HOOKS\n#define RECOMP_CALL_HOOKS 1\n'
    entries += template('entry_includes.c') + template('entry_prelude.c')
    for index, addr in enumerate(addresses):
        key = '%08x' % addr
        entries += (f'#ifndef FN_{key}\n#define FN_{key} fn_{key}\n#define RECOMP_RAW_{key}\n#endif\n'
                    f'void FN_{key}(X86 *c);\nvoid entry_{key}(X86 *c) {{\n'
                    f'#ifndef RECOMP_RAW_{key}\n    x86_cc_settle(c);\n#endif\n'
                    f'    RECOMP_ENTER({index}u, FN_{key}(c));\n}}\n')
    (out / 'chunk_entries_0d000000.c').write_text(entries)
    (out / 'fixture_overrides.h').write_text('#define FN_0d001000 fixture_native\n')
    dispatch = template('entry_dispatch.c')
    table = '''#include "x86.h"
void fn_0d001000(X86 *c);
void fn_0d001010(X86 *c);
void fn_0d001020(X86 *c);
void fn_0d001030(X86 *c);
void fixture_native(X86 *c);
const uint32_t recomp_func_addrs[] = {0x0d001000, 0x0d001010, 0x0d001020, 0x0d001030};
const uint32_t recomp_func_count = 4;
void (*const recomp_base_ptrs[])(X86 *) = {
    fixture_native, fn_0d001010, fn_0d001020, fn_0d001030
};
void (*const recomp_raw_ptrs[])(X86 *) = {
    fn_0d001000, fn_0d001010, fn_0d001020, fn_0d001030
};
uint8_t recomp_hooked[4] = {0};
uint8_t recomp_slow[4] = {0};
RecompHookFn recomp_hook_ptrs[4] = {0};
'''
    table += dispatch.replace('{signature}', 'void recomp_enter(X86 *c, uint32_t i)').replace(
        '{base_call}', '        recomp_base_ptrs[i](c);')
    table += dispatch.replace('{signature}', 'void recomp_enter_raw(X86 *c, uint32_t i, void (*raw)(X86 *))').replace(
        '{base_call}', '        raw(c);')
    (out / 'fixture_table.c').write_text(table)


if __name__ == '__main__':
    write(sys.argv[1])
