"""Validated analysis facts shared by the Python fallback and migration tools."""
import json
from pathlib import Path

from program import TranslateError

FORMAT = "recomp-analysis-facts-v1"
FORMAT_V2 = "recomp-analysis-facts-v2"
KINDS = ("call", "jump")
TOP = {"format", "executable_sha256", "evidence", "targets", "effects", "sites", "imports"}
TOP_V2 = TOP | {"inputs", "assumptions", "methods"}
INPUTS = ("producer_sha256", "code_map_sha256", "config_sha256", "catalog_sha256",
          "method_catalog_sha256")
U32 = 0xffffffff
FIELDS = frozenset(("EAX", "ECX", "EDX", "EBX", "ESP", "EBP", "ESI", "EDI",
                    "CF", "PF", "AF", "ZF", "SF", "OF"))


def _regs(value, where):
    if value in ("abi", "unknown"):
        return value
    if not isinstance(value, dict) or set(value) != {"reads", "kills", "writes", "preserves"}:
        raise TranslateError("%s: regs must be abi, unknown or a reads/kills/writes/preserves object" % where)
    out = {}
    for name, fields in value.items():
        if not isinstance(fields, list):
            raise TranslateError("%s: regs %s must list known register and flag names" % (where, name))
        normalized = []
        for field in fields:
            if not isinstance(field, str) or field.upper() not in FIELDS:
                raise TranslateError("%s: regs %s must list known register and flag names" % (where, name))
            normalized.append(field.upper())
        if len(set(normalized)) != len(normalized):
            raise TranslateError("%s: regs %s repeats a field" % (where, name))
        out[name] = frozenset(normalized)
    if out["kills"] & out["preserves"]:
        raise TranslateError("%s: regs kills and preserves overlap" % where)
    if out["kills"] - out["writes"]:
        raise TranslateError("%s: regs kills must be a subset of writes" % where)
    if out["writes"] | out["preserves"] != FIELDS:
        raise TranslateError("%s: regs writes and preserves must cover every field" % where)
    return out


def _ident(value, where):
    if type(value) is not int or not 0 <= value <= U32:
        raise TranslateError("%s: expected a u32 id, not %r" % (where, value))
    return value


def _address(value, where):
    if not isinstance(value, str) or len(value) != 8 or any(c not in "0123456789abcdefABCDEF" for c in value):
        raise TranslateError("%s: expected an 8-digit hexadecimal address, not %r" % (where, value))
    return int(value, 16)


def _digest(value, where):
    if not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdefABCDEF" for c in value):
        raise TranslateError("%s must be a SHA-256 digest" % where)
    return value.lower()


def _rows(data, name, path):
    value = data.get(name, [])
    if not isinstance(value, list):
        raise TranslateError("%s: %s must be a list" % (path, name))
    return value


def _import_name(value, path):
    if not isinstance(value, str) or value.count("!") != 1:
        raise TranslateError("%s: import must be dll!name, not %r" % (path, value))
    dll, name = value.split("!")
    if not dll or not name:
        raise TranslateError("%s: import must name a non-empty dll and symbol" % path)
    return dll.lower() + "!" + name


def _method_identity(value, path):
    if not isinstance(value, str) or value.count("!") != 1:
        raise TranslateError("%s: method identity must be dll!Interface::Method, not %r" % (path, value))
    dll, rest = value.split("!")
    if not dll or rest.count("::") != 1:
        raise TranslateError("%s: method identity must be dll!Interface::Method, not %r" % (path, value))
    interface, method = rest.split("::")
    if not interface or not method or "!" in interface:
        raise TranslateError("%s: method identity must be dll!Interface::Method, not %r" % (path, value))
    return dll.lower(), interface, method


def _inputs(value, path):
    if not isinstance(value, dict):
        raise TranslateError("%s: inputs must be an object" % path)
    if set(value) - set(INPUTS) - {"observations"}:
        raise TranslateError("%s: unexpected input keys %s"
                             % (path, ", ".join(sorted(set(value) - set(INPUTS) - {"observations"}))))
    out = {}
    for name in INPUTS:
        digest = value.get(name)
        if digest is not None:
            out[name] = _digest(digest, "%s: input %s" % (path, name))
    observations = value.get("observations", {})
    if not isinstance(observations, dict):
        raise TranslateError("%s: input observations must be an object" % path)
    out["observations"] = {}
    for label, digest in observations.items():
        if not isinstance(label, str) or not label:
            raise TranslateError("%s: observation labels must be non-empty strings" % path)
        out["observations"][label] = _digest(digest, "%s: observation %r" % (path, label))
    return out


def load(path, sha256):
    path = Path(path)
    try:
        data = json.loads(path.read_text())
    except OSError as error:
        raise TranslateError("%s: %s" % (path, error))
    except ValueError as error:
        raise TranslateError("%s: invalid JSON: %s" % (path, error))
    if not isinstance(data, dict) or data.get("format") not in (FORMAT, FORMAT_V2):
        raise TranslateError("%s: expected a %s or %s object" % (path, FORMAT, FORMAT_V2))
    version = data["format"]
    allowed = TOP_V2 if version == FORMAT_V2 else TOP
    if set(data) - allowed:
        raise TranslateError("%s: unexpected keys %s" % (path, ", ".join(sorted(set(data) - allowed))))
    if version == FORMAT_V2 and not isinstance(data.get("inputs"), dict):
        raise TranslateError("%s: v2 facts require input digests" % path)
    if not isinstance(data.get("executable_sha256"), str) or data["executable_sha256"].lower() != sha256.lower():
        raise TranslateError("%s: executable_sha256 does not match %s" % (path, sha256))
    inputs = _inputs(data["inputs"], path) if version == FORMAT_V2 else None

    evidence = {}
    for row in _rows(data, "evidence", path):
        if not isinstance(row, dict) or set(row) != {"id", "text"}:
            raise TranslateError("%s: malformed evidence record" % path)
        ident = _ident(row["id"], path)
        text = row["text"]
        if ident in evidence or not isinstance(text, str) or not text:
            raise TranslateError("%s: invalid evidence %r" % (path, ident))
        evidence[ident] = text
    used = set()

    def refs(ids, where):
        if not isinstance(ids, list):
            raise TranslateError("%s: %s evidence must be a list" % (path, where))
        out = []
        for item in ids:
            item = _ident(item, where)
            if item not in evidence:
                raise TranslateError("%s: %s references missing evidence %s" % (path, where, item))
            out.append(item)
        if len(set(out)) != len(out):
            raise TranslateError("%s: %s repeats an evidence reference" % (path, where))
        used.update(out)
        return tuple(out)

    target_sets = {}
    for row in _rows(data, "targets", path):
        if not isinstance(row, dict) or set(row) != {"id", "targets", "evidence"}:
            raise TranslateError("%s: malformed target set" % path)
        ident = _ident(row["id"], path)
        if ident in target_sets or not isinstance(row["targets"], list) or not row["targets"]:
            raise TranslateError("%s: invalid target set %r" % (path, ident))
        targets = tuple(_address(target, "%s: target set %s" % (path, ident)) for target in row["targets"])
        if len(set(targets)) != len(targets):
            raise TranslateError("%s: target set %s repeats a target" % (path, ident))
        note = refs(row["evidence"], "target set %s" % ident)
        if not note:
            raise TranslateError("%s: target set %s lacks evidence" % (path, ident))
        target_sets[ident] = targets

    effects = {}
    for row in _rows(data, "effects", path):
        if (not isinstance(row, dict) or set(row) - {"id", "regs", "x87"}
                or "id" not in row):
            raise TranslateError("%s: malformed effect record" % path)
        ident = _ident(row["id"], path)
        if ident in effects:
            raise TranslateError("%s: effect %s repeats" % (path, ident))
        fields = {name: row[name] for name in ("regs", "x87") if row.get(name) is not None}
        if not fields:
            raise TranslateError("%s: effect %s sets neither regs nor x87" % (path, ident))
        if "regs" in fields:
            fields["regs"] = _regs(fields["regs"], "%s: effect %s" % (path, ident))
        if "x87" in fields:
            x87 = fields["x87"]
            if isinstance(x87, dict):
                if (set(x87) != {"preserved", "known", "value"}
                        or any(type(x87[k]) is not int or not 0 <= x87[k] <= 0xffff for k in x87)
                        or x87["value"] & ~x87["known"]
                        or x87["known"] & x87["preserved"]):
                    raise TranslateError("%s: effect %s has invalid x87 mask" % (path, ident))
            elif x87 not in ("preserve", "unknown"):
                raise TranslateError("%s: effect %s has invalid x87 value" % (path, ident))
        effects[ident] = fields

    methods = {}
    for row in _rows(data, "methods", path):
        if (not isinstance(row, dict)
                or set(row) - {"id", "identity", "interface", "method", "slot", "argc", "effect", "evidence"}
                or "id" not in row or "identity" not in row):
            raise TranslateError("%s: malformed method record" % path)
        ident = _ident(row["id"], path)
        if ident in methods:
            raise TranslateError("%s: method %s repeats" % (path, ident))
        dll, interface, method = _method_identity(row["identity"], path)
        if row.get("interface") is not None and row["interface"] != interface:
            raise TranslateError("%s: method %s interface does not match identity" % (path, ident))
        if row.get("method") is not None and row["method"] != method:
            raise TranslateError("%s: method %s name does not match identity" % (path, ident))
        for key in ("slot", "argc"):
            if row.get(key) is not None:
                _ident(row[key], "method %s %s" % (ident, key))
        effect_id = row.get("effect")
        if effect_id is not None:
            effect_id = _ident(effect_id, "method %s" % ident)
            if effect_id not in effects:
                raise TranslateError("%s: method %s references missing effect" % (path, ident))
        note = refs(row.get("evidence", []), "method %s" % ident)
        if effect_id is not None and not note:
            raise TranslateError("%s: method %s has an effect without evidence" % (path, ident))
        methods[ident] = {"identity": "%s!%s::%s" % (dll, interface, method),
                          "interface": interface, "method": method,
                          "slot": row.get("slot"), "argc": row.get("argc"),
                          "effect": None if effect_id is None else effects[effect_id],
                          "evidence": note}

    assumptions = []
    assumption_ids = set()
    for row in _rows(data, "assumptions", path):
        if (not isinstance(row, dict) or set(row) - {"id", "text", "requires"}
                or "id" not in row or "text" not in row
                or not isinstance(row["id"], str) or not row["id"]
                or not isinstance(row["text"], str) or not row["text"]):
            raise TranslateError("%s: malformed assumption record" % path)
        ident = row["id"]
        requires = row.get("requires", [])
        if (not isinstance(requires, list)
                or any(not isinstance(value, str) for value in requires)):
            raise TranslateError("%s: assumption %s requires a list of ids" % (path, ident))
        if len(set(requires)) != len(requires):
            raise TranslateError("%s: assumption %s repeats a dependency" % (path, ident))
        if ident in assumption_ids:
            raise TranslateError("%s: assumption %s repeats" % (path, ident))
        assumption_ids.add(ident)
        assumptions.append({"id": ident, "text": row["text"],
                            "requires": sorted(set(requires))})
    missing = {required for row in assumptions for required in row["requires"]
               if required not in assumption_ids}
    if missing:
        raise TranslateError("%s: assumptions have missing dependencies %s"
                             % (path, ", ".join(sorted(missing))))
    known = set()
    while True:
        resolved = {row["id"] for row in assumptions
                    if row["id"] not in known and set(row["requires"]) <= known}
        if not resolved:
            break
        known |= resolved
    if known != assumption_ids:
        cyclic = [row["id"] for row in assumptions if row["id"] not in known]
        raise TranslateError("%s: assumptions have cyclic dependencies %s"
                             % (path, ", ".join(cyclic)))

    sites = {}
    for row in _rows(data, "sites", path):
        if not isinstance(row, dict) or set(row) - {"s", "k", "t", "e", "m", "ev", "prefer_guest"} or "s" not in row:
            raise TranslateError("%s: malformed site record" % path)
        site = _address(row["s"], path)
        if site in sites:
            raise TranslateError("%s: site %08x repeats" % (path, site))
        kind = row.get("k", "call")
        if kind not in KINDS:
            raise TranslateError("%s: site %08x has invalid kind" % (path, site))
        prefer_guest = row.get("prefer_guest", False)
        if type(prefer_guest) is not bool or prefer_guest and (version != FORMAT_V2 or kind != "call"):
            raise TranslateError("%s: site %08x has invalid guest dispatch preference" % (path, site))
        target_id = row.get("t")
        if target_id is not None:
            target_id = _ident(target_id, "site %08x" % site)
            if target_id not in target_sets:
                raise TranslateError("%s: site %08x references missing target set" % (path, site))
        effect_id = row.get("e")
        if effect_id is not None:
            effect_id = _ident(effect_id, "site %08x" % site)
            if effect_id not in effects:
                raise TranslateError("%s: site %08x references missing effect" % (path, site))
        method_id = row.get("m")
        if version == FORMAT and method_id is not None:
            raise TranslateError("%s: v1 facts cannot carry method references" % path)
        if method_id is not None:
            method_id = _ident(method_id, "site %08x" % site)
            if method_id not in methods:
                raise TranslateError("%s: site %08x references missing method" % (path, site))
        effect_evidence = refs(row.get("ev", []), "site %08x effect" % site)
        if effect_id is not None and not effect_evidence:
            raise TranslateError("%s: site %08x effect lacks evidence" % (path, site))
        if target_id is None and effect_id is None and not prefer_guest:
            raise TranslateError("%s: site %08x has neither bound, effect nor dispatch preference" % (path, site))
        if effect_id is None and effect_evidence:
            raise TranslateError("%s: site %08x has evidence without an effect" % (path, site))
        sites[site] = {"kind": kind,
                       "prefer_guest": prefer_guest,
                       "targets": None if target_id is None else target_sets[target_id],
                       "effect": None if effect_id is None else effects[effect_id],
                       "method": method_id,
                       "effect_evidence": effect_evidence}

    imports = {}
    for row in _rows(data, "imports", path):
        if (not isinstance(row, dict) or set(row) - {"name", "effect", "evidence"}
                or "name" not in row or "effect" not in row):
            raise TranslateError("%s: malformed import record" % path)
        name = _import_name(row["name"], path)
        effect_id = _ident(row["effect"], path)
        if effect_id not in effects:
            raise TranslateError("%s: import %s references missing effect" % (path, name))
        note = refs(row.get("evidence", []), "import %s" % name)
        if not note or name in imports:
            raise TranslateError("%s: invalid or repeated import %s" % (path, name))
        imports[name] = {"effect": effects[effect_id], "evidence": note}

    if set(evidence) != used:
        missing = sorted(set(evidence) - used)
        raise TranslateError("%s: unused evidence ids %s" % (path, missing))
    return {"sites": sites, "imports": imports, "evidence": evidence,
            "methods": methods, "assumptions": assumptions, "inputs": inputs,
            "target_sets": len(target_sets),
            "target_entries": sum(len(targets) for targets in target_sets.values())}
