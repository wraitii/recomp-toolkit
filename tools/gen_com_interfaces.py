#!/usr/bin/env python3
"""Generate guest COM slot tables and IIDs from pinned Wine API headers.

The JSON supplies handler choices only. Names, order and stdcall arity (including
this) come from the headers. Unsupported methods keep named import diagnostics.
Interfaces declared with an inline GUID (`DECLARE_INTERFACE_IID_`) and helper
interfaces declared with a separate `DEFINE_GUID` are both accepted. Branches
guarded by `#if D3DX_SDK_VERSION ...` are resolved against `--sdk-version` so an
interface's GUID and method list match the selected D3DX release. Unknown
declarations or stale handler names fail rather than producing a partial vtable.
--check verifies committed output without modifying it.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import struct
import uuid

CONDITION = re.compile(r'^\s*D3DX_SDK_VERSION\s*(<=|>=|==|!=|<|>)\s*(\d+)\s*$')
METHOD = re.compile(
    r'STDMETHOD(?:_\([^,]+,\s*(\w+)\s*\)|\(\s*(\w+)\s*\))\s*\((.*?)\)\s*(?:PURE\s*)?;', re.S)


def declaration_pattern(name):
    return re.compile(r'DECLARE_INTERFACE(?:_IID_|_)?\(' + re.escape(name) +
                      r',\s*\w+(?:,\s*"[^"]+")?\)\s*\{(.*?)\n\};', re.S)


def guid_pattern(name):
    return re.compile(r'DEFINE_GUID\(\s*IID_' + re.escape(name) + r'\s*,(.*?)\)\s*;', re.S)


def evaluate(condition, sdk_version):
    match = CONDITION.fullmatch(condition)
    if match is None:
        raise ValueError('unsupported preprocessor condition: ' + condition)
    operator, bound = match.groups()
    bound = int(bound)
    return {'<=': sdk_version <= bound, '>=': sdk_version >= bound, '==': sdk_version == bound,
            '!=': sdk_version != bound, '<': sdk_version < bound, '>': sdk_version > bound}[operator]


def preprocess(text, sdk_version):
    """Resolve only D3DX_SDK_VERSION branches, leaving every other directive in place."""
    output, stack = [], []

    def active():
        return all(frame['active'] for frame in stack)

    for line in text.splitlines(keepends=True):
        stripped = line.lstrip()
        if stripped.startswith('#'):
            directive = stripped[1:].strip()
            keyword = directive.split(None, 1)[0] if directive else ''
            if keyword in ('if', 'ifdef', 'ifndef'):
                condition = directive[2:].strip()
                handled = keyword == 'if' and 'D3DX_SDK_VERSION' in condition
                state = evaluate(condition, sdk_version) if handled else active()
                stack.append({'handled': handled, 'active': state, 'taken': state})
                if not handled:
                    output.append(line)
                continue
            if keyword == 'elif' and stack and stack[-1]['handled']:
                frame = stack[-1]
                frame['active'] = not frame['taken'] and evaluate(directive[4:].strip(), sdk_version)
                frame['taken'] = frame['taken'] or frame['active']
                continue
            if keyword == 'else' and stack and stack[-1]['handled']:
                frame = stack[-1]
                frame['active'] = not frame['taken']
                frame['taken'] = True
                continue
            if keyword == 'endif':
                if stack and stack.pop()['handled']:
                    continue
            output.append(line)
            continue
        if active():
            output.append(line)
    return ''.join(output)


def guid_from_fields(fields):
    parts = [part.strip() for part in fields.split(',')]
    if len(parts) != 11:
        raise ValueError('malformed GUID: ' + fields)
    first, second, third = (int(part, 16) for part in parts[:3])
    tail = bytes(int(part, 16) for part in parts[3:])
    return struct.pack('<IHH', first, second, third) + tail


def interface_iid(text, name):
    inline = re.search(r'DECLARE_INTERFACE_IID_\(' + re.escape(name) + r',\s*\w+,\s*"([^"]+)"\)', text)
    if inline is not None:
        return uuid.UUID(inline.group(1)).bytes_le
    definitions = guid_pattern(name).findall(text)
    if not definitions:
        raise ValueError('missing IID for ' + name)
    if len(definitions) != 1:
        raise ValueError(f'ambiguous IID for {name}: {len(definitions)} definitions')
    return guid_from_fields(definitions[0])


def interface_methods(text, name):
    declaration = declaration_pattern(name).search(text)
    if declaration is None:
        raise ValueError('missing interface: ' + name)
    methods = []
    for method in METHOD.finditer(declaration[1]):
        method_name = method[1] or method[2]
        args = method[3].strip()
        if args == 'THIS':
            argc = 1
        elif args.startswith('THIS_') and '(' not in args and ')' not in args:
            argc = 1 + len(args.removeprefix('THIS_').split(','))
        else:
            raise ValueError('unsupported parameter declaration: ' + args)
        methods.append((method_name, argc))
    if len(methods) != declaration[1].count('STDMETHOD') or not methods:
        raise ValueError('unparsed methods in ' + name)
    return methods


def render(references, bindings, sdk_version, suffix=''):
    raw = '\n'.join(reference.decode('utf-8') for reference in references)
    text = preprocess(raw, sdk_version)
    digest = hashlib.sha256(b''.join(references)).hexdigest()
    lines = ['// Generated by tools/gen_com_interfaces.py; do not edit.',
             '// Reference SHA256: ' + digest, '']
    status = []
    gated = any('flag_on' in item for item in bindings)
    seen = set()
    for binding in bindings:
        name, table, symbol = (binding[key] for key in ('interface', 'table', 'iid'))
        table, symbol = table + suffix, (symbol.rstrip('_') + suffix if suffix else symbol)
        if name in seen:
            raise ValueError('duplicate interface: ' + name)
        seen.add(name)
        flag_on = binding.get('flag_on', {})
        if not isinstance(flag_on, dict):
            raise ValueError('flag_on must be an object in ' + name)
        for identifier in (name, table, symbol, *binding['handlers'].values()):
            if not re.fullmatch(r'[A-Za-z_]\w*(?:<[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*>)?', identifier):
                raise ValueError('invalid C++ identifier: ' + identifier)
        declaration = declaration_pattern(name).search(text)
        if declaration is None:
            raise ValueError('missing interface: ' + name)
        methods = interface_methods(text, name)
        raw_declaration = declaration_pattern(name).search(raw)
        raw_methods = ({method[1] or method[2] for method in METHOD.finditer(raw_declaration[1])}
                       if raw_declaration is not None else {method[0] for method in methods})
        stale = set(binding['handlers']) - raw_methods
        if stale:
            raise ValueError('unknown handler methods in ' + name + ': ' + ', '.join(sorted(stale)))
        stale_flag = set(flag_on) - raw_methods
        if stale_flag:
            raise ValueError('unknown flag_on methods in ' + name + ': ' + ', '.join(sorted(stale_flag)))
        for method, kind in flag_on.items():
            if method not in binding['handlers']:
                raise ValueError('flag_on method without handler in ' + name + ': ' + method)
            if kind not in ('rust', 'native'):
                raise ValueError('flag_on kind for ' + name + '::' + method + ' must be rust or native')
        lines.append('static const uint8_t ' + symbol + '[16] = {')
        lines.append('    ' + ', '.join(f'0x{byte:02x}' for byte in interface_iid(text, name)) + ',')
        lines.extend(['};', 'static const ComMethod ' + table + '[] = {'])
        for method, argc in methods:
            handler = binding['handlers'].get(method, 'imports_unsupported')
            if gated and method not in flag_on:
                handler = 'imports_unsupported'
            lines.append(f'    {{"{method}", {argc}, {handler}}},')
        lines.extend(['};', ''])
        for method, _ in methods:
            kind = (flag_on.get(method) if gated else 'native') if method in binding['handlers'] else None
            status.append(f'| {name} | {method} | {kind or "unsupported (named)"} |')
    return '\n'.join(lines), status


def method_catalog(existing, references, bindings, dll, sdk_version):
    dll = dll.lower()
    text = preprocess("\n".join(reference.decode("utf-8") for reference in references), sdk_version)
    rows = [{"dll": dll, "interface": binding["interface"], "method": name,
             "slot": slot * 4, "argc": argc}
            for binding in bindings
            for slot, (name, argc) in enumerate(interface_methods(text, binding["interface"]))]
    kept = existing.get("methods", [])
    first = next((i for i, row in enumerate(kept) if row["dll"] == dll), len(kept))
    others = [row for row in kept if row["dll"] != dll]
    methods = others[:first] + rows + others[first:]
    hashes = dict(existing.get("references", {}))
    hashes[dll] = hashlib.sha256(b"".join(references)).hexdigest()
    hashes = dict(sorted(hashes.items()))
    combined = "".join(f"{name}:{digest}\n" for name, digest in hashes.items())
    return {"format": "recomp-com-methods-v1",
            "reference_sha256": hashlib.sha256(combined.encode()).hexdigest(),
            "references": hashes, "methods": methods}


def status_document(bindings_path, rows):
    return ('# Guest COM implementation status\n\n'
            'Generated by `tools/gen_com_interfaces.py --status-output`; do not edit.\n'
            'Source: `' + bindings_path.as_posix() + '` and the pinned Wine headers.\n\n'
            '`rust` routes the method through CXX to Rust; `native` is C++ that remains the\n'
            'implementation; `unsupported (named)` stops the run with `dll!Interface::Method`.\n\n'
            '| Interface | Method | Status |\n'
            '| --- | --- | --- |\n' + '\n'.join(rows) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference', type=Path, nargs='+', required=True)
    parser.add_argument('--bindings', type=Path, required=True)
    parser.add_argument('--sdk-version', type=int, default=33)
    parser.add_argument('--suffix', default='')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--catalog-output', type=Path)
    parser.add_argument('--dll')
    parser.add_argument('--status-output', type=Path)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    if not args.output and not args.catalog_output:
        parser.error("--output or --catalog-output is required")
    if args.catalog_output and not args.dll:
        parser.error("--catalog-output requires --dll")
    try:
        references = [path.read_bytes() for path in args.reference]
        bindings = json.loads(args.bindings.read_text())
        generated, rows = render(references, bindings, args.sdk_version, args.suffix)
        outputs = [(args.output, generated)] if args.output else []
        if args.catalog_output:
            existing = (json.loads(args.catalog_output.read_text())
                        if args.catalog_output.exists() else {})
            catalog = method_catalog(existing, references, bindings, args.dll, args.sdk_version)
            outputs.append((args.catalog_output, json.dumps(catalog, indent=2) + "\n"))
        if args.status_output is not None:
            outputs.append((args.status_output, status_document(args.bindings, rows)))
        stale = [str(path) for path, content in outputs
                 if not path.exists() or path.read_text() != content]
        if args.check:
            if stale:
                parser.error(', '.join(stale) + ' stale; regenerate')
            return
        for path, content in outputs:
            if not path.exists() or path.read_text() != content:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
    except (ValueError, OSError, KeyError) as failure:
        parser.error(str(failure))


if __name__ == '__main__':
    main()
