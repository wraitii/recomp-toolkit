"""Stack bytes each runtime import shim pops, read from the ImportShim tables in the kit sources.

Every table entry must parse, either as a braced initializer or as a call to a function-like
macro defined earlier in the same file; anything else stops the build with its location.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCES = ("runtime", "dx")
TOKENS = re.compile(r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|//[^\n]*|/\*.*?\*/', re.S)
DEFINE = re.compile(r"^[ \t]*#[ \t]*define[ \t]+(\w+)\(([^)]*)\)[ \t]*(.*)$", re.M)
UNDEF = re.compile(r"^[ \t]*#[ \t]*undef[ \t]+(\w+)", re.M)
TABLE = re.compile(r"\bImportShim\s+\w+\s*\[\s*\]\s*=\s*\{")
CALL = re.compile(r"(\w+)\s*\((.*)\)\s*$", re.S)
STRINGS = re.compile(r'^(?:\s*"(?:\\.|[^"\\])*")+\s*$')
SENTINELS = {"ARGC_CDECL", "ARGC_UNKNOWN"}


def strip(text):
    def blank(match):
        token = match.group(0)
        return token if token[0] in "\"'" else re.sub(r"[^\n]", " ", token)
    return TOKENS.sub(blank, text.replace("\\\n", "  "))


def split(text):
    parts, depth, start, at = [], 0, 0, 0
    while at < len(text):
        char = text[at]
        if char in "\"'":
            at = TOKENS.match(text, at).end()
            continue
        if char in "({[":
            depth += 1
        elif char in ")}]":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(text[start:at].strip())
            start = at + 1
        at += 1
    tail = text[start:].strip()
    return parts + [tail] if tail else parts


def closing(text, at):
    depth = 0
    while at < len(text):
        char = text[at]
        if char in "\"'":
            at = TOKENS.match(text, at).end()
            continue
        depth += char == "{"
        depth -= char == "}"
        if depth == 0:
            return at
        at += 1
    raise ValueError("unterminated ImportShim table")


def expand(macro, args):
    params, body = macro
    if len(params) != len(args):
        raise ValueError("macro expects %d arguments" % len(params))
    values = dict(zip(params, args))
    body = re.sub(r"#\s*(\w+)", lambda m: '"%s"' % values[m.group(1)] if m.group(1) in values else m.group(0), body)
    body = re.sub(r"\b\w+\b", lambda m: values.get(m.group(0), m.group(0)), body)
    return re.sub(r"\s*##\s*", "", body)


def literal(text):
    if not STRINGS.match(text):
        raise ValueError("expected string literals, got %r" % text)
    return "".join(re.findall(r'"((?:\\.|[^"\\])*)"', text))


def argc(text):
    if text in SENTINELS:
        return text
    if not re.fullmatch(r"[\d\s()+*/-]+", text):
        raise ValueError("unsupported argument count %r" % text)
    value = eval(text, {"__builtins__": {}})
    if value != int(value) or value < 0:
        raise ValueError("argument count %r is not a whole number" % text)
    return int(value)


def macros_at(text, at):
    directives = sorted(list(DEFINE.finditer(text, 0, at)) + list(UNDEF.finditer(text, 0, at)), key=lambda m: m.start())
    macros = {}
    for directive in directives:
        if directive.re is UNDEF:
            macros.pop(directive.group(1), None)
        else:
            params = [param.strip() for param in directive.group(2).split(",") if param.strip()]
            macros[directive.group(1)] = (params, directive.group(3).strip())
    return macros


def entries(path):
    text = strip(path.read_text(errors="replace"))
    for table in TABLE.finditer(text):
        start = table.end()
        end = closing(text, start - 1)
        body = re.sub(r"^[ \t]*#[ \t]*(define|undef)\b.*$", lambda m: " " * len(m.group(0)), text[start:end], flags=re.M)
        if re.search(r"^[ \t]*#", body, re.M):
            raise ValueError("%s:%d: ImportShim table contains conditional preprocessor lines" % (path, text.count("\n", 0, start) + 1))
        offset = 0
        for entry in split(body):
            offset = body.index(entry, offset)
            at = start + offset
            line = text.count("\n", 0, at) + 1
            try:
                call = CALL.match(entry)
                macros = macros_at(text, at)
                if not entry.startswith("{") and call and call.group(1) in macros:
                    entry = expand(macros[call.group(1)], split(call.group(2))).strip()
                if not (entry.startswith("{") and entry.endswith("}")):
                    raise ValueError("unrecognised entry %r" % entry)
                fields = split(entry[1:-1])
                if len(fields) < 3:
                    raise ValueError("entry %r has fewer than three fields" % entry)
                if fields[0] in ("nullptr", "NULL", "0"):
                    continue
                name, count = literal(fields[1]), argc(fields[2])
                decorated = re.fullmatch(r"_\w+@(\d+)", name)
                if decorated and count != "ARGC_UNKNOWN" and int(decorated.group(1)) != (0 if count == "ARGC_CDECL" else 4 * count):
                    raise ValueError("%s pops %r, not its decorated byte count" % (name, count))
                yield literal(fields[0]), name, count
            except ValueError as error:
                raise ValueError("%s:%d: %s" % (path, line, error)) from None


def import_pops(root=ROOT):
    seen = {}
    for directory in SOURCES:
        for path in sorted((root / directory).glob("*.cpp")):
            for dll, name, count in entries(path):
                seen.setdefault("%s!%s" % (dll.lower(), name), set()).add(count)
    pops = {}
    for key, counts in seen.items():
        if len(counts) != 1 or "ARGC_UNKNOWN" in counts:
            continue
        count = counts.pop()
        pops[key] = 0 if count == "ARGC_CDECL" else 4 * count
    return dict(sorted(pops.items()))
