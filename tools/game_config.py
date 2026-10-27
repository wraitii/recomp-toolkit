"""Load a game directory: games/<id>/game.toml, with an optional curated globals file.

This module is the only place that knows the schema. Python 3.9 has no
tomllib, so the pinned tomli is the fallback."""

import os
import json
from pathlib import Path
import re
import ntpath
import struct

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    import tomli as tomllib

KIT = Path(__file__).resolve().parents[1]
REQUIRED_GAME_KEYS = ("id", "name", "app_name", "bundle_id", "executable", "sha256",
                      "image_base", "entry_point", "guest_root")

HEAP_BASE_DEFAULT = 0x01000000

# The settings page's rows, in mods/display_settings.h DisplayRow order; the
# controls' seven rows are one entry. [settings] rows names the ones a game
# shows. Without the key a game shows every row.
SETTINGS_ROWS = ("window", "performance_overlay", "controls")

# [controls] default_layout and the on-screen layout picker's choices.
CONTROLS_LAYOUTS = ("pad", "keys", "pad+keys", "hidden")
# [controls] mobile_control: where the on-screen controls may appear.
MOBILE_CONTROLS = ("off", "mobile-only", "always")
# [controls.native] buttons: the pad button names a physical button maps to.
PAD_BUTTONS = ("cross", "circle", "square", "triangle", "l1", "r1", "l2", "r2", "l3", "r3",
               "select", "start", "ps")
# [controls.native] axes: the six DirectInput/XInput axis names a physical
# axis maps to.
NATIVE_AXES = ("x", "y", "z", "rx", "ry", "rz")
HEAP_END = 0x0e000000        # default heap end; reserved low runtime regions start here
GUEST_SIZE_DEFAULT = 0x10000000   # runtime/x86.h GUEST_SIZE: the arena, 256 MB unless a module needs more
AUX_REQUIRED_KEYS = ("name", "sha256", "base", "size")

GUID_RE = re.compile(
    r"^\{?([0-9a-fA-F]{8})-([0-9a-fA-F]{4})-([0-9a-fA-F]{4})-"
    r"([0-9a-fA-F]{4})-([0-9a-fA-F]{12})\}?$")


def parse_guid(value, source, where):
    """A GUID string as the 16 little-endian bytes the guest memory holds."""
    match = GUID_RE.match(value) if isinstance(value, str) else None
    if not match:
        raise ValueError("%s: %s must be a GUID, not %r" % (source, where, value))
    a, b, c, d, e = match.groups()
    return (struct.pack("<IHH", int(a, 16), int(b, 16), int(c, 16))
            + bytes.fromhex(d) + bytes.fromhex(e))

# The production translation profile: every optimization the kit has. load()
# fills these when a game omits them. fault_state = "exact" gives up the
# relaxed SSA scalar/state policies.
TRANSLATE_DEFAULTS = {
    "fault_state": "relaxed",
    "msvc_x87_convention": True,
    "call_contracts": True,
    "assume": [],
    "lazy_stack_writes": True,
    "private_leaf_contracts": False,
    "private_input_arguments": False,
    "private_input_registers": {},
    "x87_cw_clone": True,
}
ASSUMPTIONS = frozenset(("private-stack", "stack-pointer-bases"))
PRIVATE_INPUT_REGISTERS = frozenset(("EAX", "ECX", "EDX", "EBX", "EBP", "ESI", "EDI", "ST0"))

def add_game_args(parser):
    """--game <id> for a kit game under games/, or --game-dir for any directory holding game.toml."""
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--game", help="A game under the kit's games/ directory")
    group.add_argument("--game-dir", type=Path, help="The directory holding game.toml")
    parser.add_argument("--build-root", type=Path, default=None,
                        help="Where outputs, the installation link live")


def resolve_game_args(args, default="stub"):
    """Fill args.game_dir and args.build_root, exporting the build root to child tools."""
    if args.game_dir is None:
        if not (args.game or default):
            raise SystemExit("Pass --game <id> or --game-dir <directory>")
        args.game_dir = KIT / "games" / (args.game or default)
    args.game_dir = args.game_dir.resolve()
    if args.build_root is not None:
        os.environ["RECOMP_BUILD_ROOT"] = str(args.build_root.resolve())
    args.build_root = build_root_for(args.game_dir)
    return args


def build_root_for(game_dir):
    """RECOMP_BUILD_ROOT when set; else beside a game outside the kit, or build/<id> for a kit game."""
    if os.environ.get("RECOMP_BUILD_ROOT"):
        return Path(os.environ["RECOMP_BUILD_ROOT"])
    game_dir = Path(game_dir).resolve()
    try:
        game_dir.relative_to(KIT)
    except ValueError:
        return game_dir / "build"
    return KIT / "build" if game_dir == KIT / "games/stub" else KIT / "build" / game_dir.name


def windows_version(value):
    """Decode major.minor[.build]; keep the historical 9x default and 6.1 SP1."""
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+\.[0-9]+(?:\.[0-9]+)?", value):
        raise ValueError("[game] windows_version must be major.minor[.build]")
    parts = [int(part) for part in value.split(".")]
    major, minor = parts[:2]
    build = parts[2] if len(parts) == 3 else {(4, 10): 2222, (6, 1): 7601}.get((major, minor), 0)
    if major > 255 or minor > 255 or build > 32767:
        raise ValueError("[game] windows_version requires byte-sized major/minor and a 15-bit build")
    return major, minor, build, 2 if major >= 5 else 1


def validate_heap_base(value, heap_end=HEAP_END):
    """The heap arena start: page aligned, above the image base, below the arena end."""
    if value % 0x1000 or not (0x00400000 < value < heap_end):
        raise ValueError("[game] heap_base %#x must be page aligned and between 0x00400000 and %#x" % (value, heap_end))
    return value


def relative_game_path(value, source, where, allow_parent=False):
    if not isinstance(value, str) or not value or "\0" in value:
        raise ValueError("%s: %s must be a non-empty relative path" % (source, where))
    value = value.replace("\\", "/")
    drive, _ = ntpath.splitdrive(value)
    if drive or value.startswith("/"):
        raise ValueError("%s: %s must be relative to the game content root" % (source, where))
    parts = []
    for part in value.split("/"):
        if not part or part == ".":
            continue
        if part == "..":
            if not allow_parent or not parts:
                raise ValueError("%s: %s escapes the game content root" % (source, where))
            parts.pop()
        else:
            parts.append(part)
    if not parts:
        return "."
    return "/".join(parts)


def relative_path_list(value, source, where):
    if not isinstance(value, list):
        raise ValueError("%s: %s must be a list of relative paths" % (source, where))
    paths = [relative_game_path(path, source, "%s[%d]" % (where, index))
             for index, path in enumerate(value)]
    if len(set(paths)) != len(paths):
        raise ValueError("%s: %s contains duplicate paths" % (source, where))
    return paths


def validate_installation_hints(cfg, source):
    game = cfg["game"]
    launcher = cfg["launcher"]
    candidates = launcher.setdefault("candidate_executables", [game["executable"]])
    if (not isinstance(candidates, list) or not candidates
            or any(not isinstance(name, str) or not name or "/" in name or "\\" in name
                   or ":" in name or name in (".", "..") or "\0" in name for name in candidates)):
        raise ValueError("%s: [launcher] candidate_executables must be a non-empty list of file names" % source)
    if len({name.lower() for name in candidates}) != len(candidates):
        raise ValueError("%s: [launcher] candidate_executables contains duplicates" % source)
    if game["executable"].lower() not in {name.lower() for name in candidates}:
        candidates.insert(0, game["executable"])

    setup = cfg["setup"]
    downloads = setup.setdefault("downloads", [])
    if not isinstance(downloads, list):
        raise ValueError("%s: setup.downloads must be an array of tables" % source)
    destinations = set()
    for item in downloads:
        if (not isinstance(item, dict)
                or any(not isinstance(item.get(key), str) or not item[key]
                       for key in ("label", "path", "member", "url"))
                or not item["url"].startswith("https://")
                or any(type(item.get(key)) is not int or not 0 < item[key] <= 64 * 1024 * 1024
                       for key in ("size", "archive_size"))
                or any(not isinstance(item.get(key), str) or not re.fullmatch(r"[0-9a-f]{64}", item[key])
                       for key in ("sha256", "archive_sha256"))):
            raise ValueError("%s: setup.downloads requires a label, HTTPS url, member, path, sizes and SHA-256 hashes" % source)
        path = relative_game_path(item["path"], source, "setup.downloads.path")
        if "/" in path or path == "." or path.casefold() in destinations:
            raise ValueError("%s: setup.downloads.path must be a unique filename beside the executable" % source)
        if item["member"][0] in "-@" or any(ord(c) < 32 for c in item["member"]):
            raise ValueError("%s: setup.downloads.member must be an archive member name" % source)
        item["path"] = path
        destinations.add(path.casefold())
    for key in ("supported_version", "source_notes"):
        value = setup.setdefault(key, "")
        if not isinstance(value, str):
            raise ValueError("%s: [setup] %s must be a string" % (source, key))
    installers = setup.setdefault("installers", [])
    if not isinstance(installers, list):
        raise ValueError("%s: setup.installers must be an array of tables" % source)
    for installer in installers:
        if (not isinstance(installer, dict) or installer.get("format") != "inno"
                or not isinstance(installer.get("sha256"), str)
                or not re.fullmatch(r"[0-9a-fA-F]{64}", installer["sha256"])
                or any(type(installer.get(key)) is not int or installer[key] <= 0
                       for key in ("size", "expanded_size"))):
            raise ValueError("%s: each setup.installers entry needs format=inno, sha256, size and expanded_size" % source)
        moves = installer.setdefault("move_files", {})
        if not isinstance(moves, dict):
            raise ValueError("%s: setup.installers move_files must map source paths to destination paths" % source)
        for old, new in moves.items():
            for path in (old, new):
                if relative_game_path(path, source, "setup.installers.move_files") == ".":
                    raise ValueError("%s: setup.installers move_files must name files" % source)
    layout = setup.setdefault("layout", {})
    if not isinstance(layout, dict):
        raise ValueError("%s: [setup] layout must be a table" % source)
    for key in ("executable_dir", "data_root"):
        layout[key] = relative_game_path(layout.get(key, "."), source,
                                         "[setup.layout] " + key)
    for key in ("required_dirs", "required_files"):
        layout[key] = relative_path_list(layout.get(key, []), source,
                                         "[setup.layout] " + key)

    legacy_dirs = setup.setdefault("required_dirs", [])
    if not isinstance(legacy_dirs, list):
        raise ValueError("%s: [setup] required_dirs must be a list" % source)
    for index, path in enumerate(legacy_dirs):
        if not isinstance(path, str) or not path:
            raise ValueError("%s: [setup] required_dirs[%d] must be a non-empty path" % (source, index))
        if path.startswith(("/", "\\")) or ntpath.splitdrive(path)[0]:
            raise ValueError("%s: [setup] required_dirs[%d] must be relative" % (source, index))
        relative_game_path(layout["executable_dir"] + "/" + path, source,
                           "[setup] required_dirs[%d]" % index, allow_parent=True)
    for index, path in enumerate(cfg["fonts"]["files"]):
        if not isinstance(path, str) or not path:
            raise ValueError("%s: [fonts] files[%d] must be a non-empty path" % (source, index))
        normalized = path.replace("\\", "/")
        if normalized.startswith("/") or ntpath.splitdrive(normalized)[0]:
            raise ValueError("%s: [fonts] files[%d] must be relative" % (source, index))
        relative_game_path(layout["executable_dir"] + "/" + path, source,
                           "[fonts] files[%d]" % index, allow_parent=True)

    versions = setup.setdefault("versions", {})
    if not isinstance(versions, dict):
        raise ValueError("%s: [setup] versions must be a table" % source)
    statuses = {"supported", "preparation_required", "recognized_unsupported", "incomplete"}
    for key, version in versions.items():
        where = "[setup.versions.%s]" % key
        if not isinstance(version, dict):
            raise ValueError("%s: %s must be a table" % (source, where))
        label, size, digest = (version.get(field) for field in ("label", "size", "sha256"))
        if not isinstance(label, str) or not label:
            raise ValueError("%s: %s label must be a non-empty string" % (source, where))
        if type(size) is not int or size <= 0:
            raise ValueError("%s: %s size must be a positive integer" % (source, where))
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
            raise ValueError("%s: %s sha256 must contain 64 hexadecimal digits" % (source, where))
        if version.get("status") not in statuses:
            raise ValueError("%s: %s status must be one of %s" % (source, where, ", ".join(sorted(statuses))))
        preparations = version.setdefault("preparations", [])
        if not isinstance(preparations, list) or not all(isinstance(v, str) and v for v in preparations):
            raise ValueError("%s: %s preparations must be a list of IDs" % (source, where))

    preparations = setup.setdefault("preparations", {})
    if not isinstance(preparations, dict):
        raise ValueError("%s: [setup] preparations must be a table" % source)
    for key, preparation in preparations.items():
        where = "[setup.preparations.%s]" % key
        if not isinstance(preparation, dict) or preparation.get("mode") != "manual":
            raise ValueError("%s: %s currently requires mode = 'manual'" % (source, where))
        explanation = preparation.get("explanation")
        if not isinstance(explanation, str) or not explanation:
            raise ValueError("%s: %s explanation must be a non-empty string" % (source, where))
        for field in ("applies_to", "required_inputs", "required_tools", "outputs"):
            values = preparation.get(field, [])
            if not isinstance(values, list) or not all(isinstance(v, str) and v for v in values):
                raise ValueError("%s: %s %s must be a list of non-empty strings" % (source, where, field))
            preparation[field] = values
        if not preparation["outputs"]:
            raise ValueError("%s: %s outputs must name at least one validated artifact role" % (source, where))
        if any(v not in versions for v in preparation["applies_to"]):
            raise ValueError("%s: %s applies_to names an unknown version" % (source, where))
    for key, version in versions.items():
        for preparation in version["preparations"]:
            if preparation not in preparations or key not in preparations[preparation]["applies_to"]:
                raise ValueError("%s: [setup.versions.%s] references an unknown or inapplicable preparation %s"
                                 % (source, key, preparation))

    required_roles = [{"role": "executable",
                       "path": relative_game_path(layout["executable_dir"] + "/" + game["executable"],
                                                  source, "[game] executable"),
                       "kind": "file", "sha256": game["sha256"]}]
    for module in cfg["aux_modules"]:
        declared = cfg.get("modules", {}).get("aux", {}).get(module["key"], {})
        original = declared.get("original_path", module["name"])
        original = relative_game_path(original, source,
                                      "[modules.aux.%s] original_path" % module["key"])
        required_roles.append({"role": "auxiliary:" + module["key"],
                               "path": relative_game_path(layout["executable_dir"] + "/" + original,
                                                          source, "[modules.aux.%s]" % module["key"]),
                               "kind": "file", "sha256": module["sha256"]})
    for path in layout["required_files"]:
        required_roles.append({"role": "data:" + path, "path": path, "kind": "file"})
    for path in layout["required_dirs"]:
        required_roles.append({"role": "data:" + path, "path": path, "kind": "directory"})
    for path in setup["required_dirs"]:
        normalized = relative_game_path(layout["executable_dir"] + "/" + path, source,
                                        "[setup] required_dirs", allow_parent=True)
        required_roles.append({"role": "data:" + normalized, "path": normalized, "kind": "directory"})
    for path in cfg["fonts"]["files"]:
        path = relative_game_path(layout["executable_dir"] + "/" + path, source, "[fonts] files",
                                  allow_parent=True)
        required_roles.append({"role": "data:" + path, "path": path, "kind": "file"})
    deduped_roles = {}
    for artifact in required_roles:
        previous = deduped_roles.get(artifact["role"])
        if previous and previous["kind"] != artifact["kind"]:
            raise ValueError("%s: artifact role %s is both a file and a directory" % (source, artifact["role"]))
        deduped_roles[artifact["role"]] = artifact
    required_roles = list(deduped_roles.values())
    setup["required_roles"] = required_roles
    artifacts = dict(deduped_roles)
    for key, preparation in preparations.items():
        for role in preparation["outputs"]:
            if role not in artifacts:
                raise ValueError("%s: [setup.preparations.%s] outputs names an unknown artifact role %s"
                                 % (source, key, role))

    checklist = setup.setdefault("checklist", {})
    if not isinstance(checklist, dict):
        raise ValueError("%s: [setup] checklist must be a table" % source)
    items = checklist.setdefault("items", [])
    if not isinstance(items, list):
        raise ValueError("%s: [setup.checklist] items must be a list" % source)
    seen_ids = set()
    aux_keys = {module["key"] for module in cfg["aux_modules"]}
    for index, item in enumerate(items):
        where = "[setup.checklist.items[%d]]" % index
        if not isinstance(item, dict):
            raise ValueError("%s: %s must be a table" % (source, where))
        item_id, role = item.get("id"), item.get("role")
        if not isinstance(item_id, str) or not item_id or item_id in seen_ids:
            raise ValueError("%s: %s id must be a unique non-empty string" % (source, where))
        seen_ids.add(item_id)
        if role == "executable":
            pass
        elif isinstance(role, str) and role.startswith("auxiliary:"):
            if role.partition(":")[2] not in aux_keys:
                raise ValueError("%s: %s role names an unknown auxiliary module" % (source, where))
        elif isinstance(role, str) and role.startswith("data:"):
            relative_game_path(role.partition(":")[2], source, where + " role")
        else:
            raise ValueError("%s: %s role must be executable, auxiliary:<id>, or data:<path>" % (source, where))
        if role.startswith("data:"):
            path = relative_game_path(role.partition(":")[2], source, where + " role")
            item["role"] = role = "data:" + path
            artifacts.setdefault(role, {"role": role, "path": path, "kind": "file"})
        if role not in artifacts:
            raise ValueError("%s: %s role is not a declared artifact" % (source, where))
        for field in ("purpose", "acquisition"):
            if not isinstance(item.get(field), str) or not item[field]:
                raise ValueError("%s: %s %s must be a non-empty string" % (source, where, field))
        if not isinstance(item.get("required", True), bool):
            raise ValueError("%s: %s required must be a boolean" % (source, where))
        if (not isinstance(item.get("accepted_forms", []), list) or not item.get("accepted_forms", [])
                or not all(isinstance(v, str) and v for v in item.get("accepted_forms", []))):
            raise ValueError("%s: %s accepted_forms must be a non-empty list of non-empty strings" % (source, where))
        if role in {artifact["role"] for artifact in required_roles} and not item.get("required", True):
            raise ValueError("%s: %s cannot make a required artifact optional" % (source, where))
    explicit = {}
    for item in items:
        if item["role"] in explicit:
            raise ValueError("%s: [setup.checklist] repeats role %s" % (source, item["role"]))
        explicit[item["role"]] = item
    normalized_items = []
    for artifact in required_roles:
        role = artifact["role"]
        item = explicit.pop(role, None)
        if item is None:
            item = {"id": role, "role": role,
                    "purpose": "Required game content.",
                    "acquisition": "Locate this item in your own game copy.",
                    "accepted_forms": ["Exact fingerprint declared by this artifact role"
                                       if artifact.get("sha256") else
                                       ("Directory at " if artifact["kind"] == "directory" else "File at ")
                                       + artifact["path"]]}
        item.update({key: value for key, value in artifact.items() if key in ("path", "kind", "sha256")})
        item["required"] = True
        normalized_items.append(item)
    for role, item in explicit.items():
        artifact = artifacts[role]
        item.update({key: value for key, value in artifact.items() if key in ("path", "kind", "sha256")})
        item.setdefault("required", False)
        normalized_items.append(item)
    normalized_ids = [item["id"] for item in normalized_items]
    if len(set(normalized_ids)) != len(normalized_ids):
        raise ValueError("%s: [setup.checklist] contains duplicate item IDs" % source)
    checklist["items"] = normalized_items


def load_controls(controls, source):
    """Validate [controls] and merge [controls.native] over its defaults."""
    controls = dict(controls)
    controls.setdefault("default_layout", "pad")
    if controls["default_layout"] not in CONTROLS_LAYOUTS:
        raise ValueError("%s: [controls] default_layout must be one of %s, not %r"
                         % (source, ", ".join(CONTROLS_LAYOUTS), controls["default_layout"]))
    mobile = controls.setdefault("mobile_control", "mobile-only")
    if mobile not in MOBILE_CONTROLS:
        raise ValueError("%s: [controls] mobile_control must be one of %s, not %r"
                         % (source, ", ".join(MOBILE_CONTROLS), mobile))
    unknown_controls = sorted(k for k in controls
                              if k not in ("default_layout", "mobile_control", "native"))
    if unknown_controls:
        raise ValueError("%s: [controls] may name only default_layout, mobile_control and native, not %s"
                         % (source, ", ".join(unknown_controls)))

    native = dict(controls.get("native", {}))
    unknown_native = sorted(k for k in native if k not in ("xinput", "dinput", "axes", "buttons"))
    if unknown_native:
        raise ValueError("%s: [controls.native] may name only xinput, dinput, axes, buttons, not %s"
                         % (source, ", ".join(unknown_native)))
    native.setdefault("xinput", True)
    native.setdefault("dinput", True)
    if not isinstance(native["xinput"], bool) or not isinstance(native["dinput"], bool):
        raise ValueError("%s: [controls.native] xinput and dinput must be booleans" % source)
    axes = native.setdefault("axes", ["x", "y", "z", "rz", "rx", "ry"])
    if not isinstance(axes, list) or sorted(axes) != sorted(NATIVE_AXES):
        raise ValueError("%s: [controls.native] axes must list all six of %s exactly once, not %r"
                         % (source, ", ".join(NATIVE_AXES), axes))
    buttons = native.setdefault(
        "buttons", ["square", "cross", "circle", "triangle", "l1", "r1", "l2", "r2", "select",
                   "start", "l3", "r3", "ps"])
    if not isinstance(buttons, list) or sorted(buttons) != sorted(PAD_BUTTONS):
        raise ValueError("%s: [controls.native] buttons must list all thirteen of %s exactly once, not %r"
                         % (source, ", ".join(PAD_BUTTONS), buttons))
    controls["native"] = native
    return controls


def load(game_dir, build_root=None):
    """Return the parsed config with `globals` merged in and `dir`/`source` recorded."""
    game_dir = Path(game_dir)
    build_root = Path(build_root) if build_root is not None else build_root_for(game_dir)
    source = game_dir / "game.toml"
    with source.open("rb") as fh:
        cfg = tomllib.load(fh)
    game = cfg.get("game", {})
    missing = [key for key in REQUIRED_GAME_KEYS if key not in game]
    if missing:
        raise ValueError("%s: missing [game] keys: %s" % (source, ", ".join(missing)))
    game["heap_end"] = int(game.get("heap_end", HEAP_END))
    game["heap_base"] = validate_heap_base(int(game.get("heap_base", HEAP_BASE_DEFAULT)),
                                           game["heap_end"])
    windows_version(game.setdefault("windows_version", "4.10"))
    # Fail loudly rather than returning 0 from an import whose stdcall arity is
    # unknown: the un-popped arguments otherwise drift the guest stack.
    # Store hooks (the guest watchpoint and DirectDraw dirty tracking) cost a
    # test and an aliasing reload on every guest store. Off, RECOMP_WATCH is
    # unavailable and a DirectDraw Unlock compares the whole surface.
    if not isinstance(game.setdefault("store_hooks", True), bool):
        raise ValueError("%s: [game] store_hooks must be a boolean" % source)
    if not isinstance(game.setdefault("call_hooks", True), bool):
        raise ValueError("%s: [game] call_hooks must be a boolean" % source)
    strict_imports = game.setdefault("strict_imports", False)
    if not isinstance(strict_imports, bool):
        raise ValueError("%s: [game] strict_imports must be a boolean" % source)
    registry = cfg.setdefault("registry", {})
    hives = ("HKEY_CLASSES_ROOT", "HKEY_CURRENT_USER", "HKEY_LOCAL_MACHINE",
             "HKEY_USERS", "HKEY_CURRENT_CONFIG")
    if not isinstance(registry, dict):
        raise ValueError("%s: [registry] must contain registry key tables" % source)
    for key, values in registry.items():
        if key.split("\\")[0] not in hives or "\0" in key or not isinstance(values, dict):
            raise ValueError("%s: invalid registry key %r" % (source, key))
        for name, value in values.items():
            if "\0" in name or not (isinstance(value, str) and "\0" not in value
                                   or type(value) is int and 0 <= value <= 0xffffffff):
                raise ValueError("%s: registry value %s\\%s must be a string or DWORD" %
                                 (source, key, name))
    environment = cfg.setdefault("environment", {})
    passthrough = environment.setdefault("passthrough", [])
    if not isinstance(passthrough, list) or any(
            not isinstance(name, str) or not name or any(ch in name for ch in "=\0\r\n")
            for name in passthrough):
        raise ValueError("%s: [environment] passthrough must be a list of environment variable names" % source)
    if len({name.upper() for name in passthrough}) != len(passthrough):
        raise ValueError("%s: [environment] passthrough contains duplicate names" % source)
    translate = cfg.setdefault("translate", {})
    if translate.get("engine", "rust") not in ("python", "rust"):
        raise ValueError('%s: [translate] engine must be "python" or "rust"' % source)
    # "relaxed" keeps CPU and x87 state in host locals between observation
    # points, so an interior fault may see stale scratch state (DIVERGENCE
    # tags cpu-locals, ssa-x87-scalar, ssa-state-locals); "exact" publishes it
    # at every instruction. Null-check builds always use the exact form.
    if translate.setdefault("fault_state", TRANSLATE_DEFAULTS["fault_state"]) not in ("relaxed", "exact"):
        raise ValueError('%s: [translate] fault_state must be "relaxed" or "exact"' % source)
    # Calls and returns follow the MSVC x87 stack convention: popped x87
    # residue is dead there (DIVERGENCE ssa-x87-convention). Arithmetic flags
    # stay published either way. False restores conservative publication.
    if not isinstance(translate.setdefault("msvc_x87_convention", TRANSLATE_DEFAULTS["msvc_x87_convention"]), bool):
        raise ValueError("%s: [translate] msvc_x87_convention must be a boolean" % source)
    # Per-function reads/kills contracts let a direct CALL omit publication of
    # a field the callee neither reads nor preserves (DIVERGENCE
    # ssa-call-contracts). False publishes every field at every call.
    if not isinstance(translate.setdefault("call_contracts", TRANSLATE_DEFAULTS["call_contracts"]), bool):
        raise ValueError("%s: [translate] call_contracts must be a boolean" % source)
    # Opt-in premises about original code; docs/translator.md lists what each assumes.
    assume = translate.setdefault("assume", list(TRANSLATE_DEFAULTS["assume"]))
    if (not isinstance(assume, list) or not set(assume) <= ASSUMPTIONS
            or len(set(assume)) != len(assume)):
        raise ValueError("%s: [translate] assume must list unique names from %s" % (source, ", ".join(sorted(ASSUMPTIONS))))
    if type(translate.setdefault("lazy_stack_writes", TRANSLATE_DEFAULTS["lazy_stack_writes"])) is not bool:
        raise ValueError("%s: [translate] lazy_stack_writes must be a boolean" % source)
    for key in ("exposed_stack_functions", "stack_helpers", "stack_probes"):
        listed = translate.setdefault(key, [])
        if (not isinstance(listed, list)
                or any(type(v) is not int or not 0 <= v <= 0xffffffff for v in listed)
                or len(set(listed)) != len(listed)):
            raise ValueError("%s: [translate] %s must be a list of unique 32-bit addresses" % (source, key))
    # Stack bytes a call site or function pops, where cleanup inference cannot prove them.
    stack_cleanup = translate.setdefault("stack_cleanup", [])
    if (not isinstance(stack_cleanup, list)
            or any(not isinstance(row, dict) or len(row) != 3
                   or not set(row) - {"bytes", "evidence"} <= {"call", "function"}
                   or type(row.get("bytes")) is not int or not 0 <= row["bytes"] <= 0xffff or row["bytes"] % 4
                   or type(row.get("call", row.get("function"))) is not int
                   or not 0 <= row.get("call", row.get("function")) <= 0xffffffff
                   or not isinstance(row.get("evidence"), str) or not row["evidence"].strip()
                   for row in stack_cleanup)
            or len({(row.get("call"), row.get("function")) for row in stack_cleanup}) != len(stack_cleanup)):
        raise ValueError("%s: [translate] stack_cleanup must be a list of unique {call|function, bytes, evidence} with dword bytes" % source)
    if not isinstance(translate.setdefault("x87_cw_clone", TRANSLATE_DEFAULTS["x87_cw_clone"]), bool):
        raise ValueError("%s: [translate] x87_cw_clone must be a boolean" % source)
    if not isinstance(translate.setdefault("private_leaf_contracts", TRANSLATE_DEFAULTS["private_leaf_contracts"]), bool):
        raise ValueError("%s: [translate] private_leaf_contracts must be a boolean" % source)
    if type(translate.setdefault("private_input_arguments", TRANSLATE_DEFAULTS["private_input_arguments"])) is not bool:
        raise ValueError("%s: [translate] private_input_arguments must be a boolean" % source)
    dispatch = translate.get("dispatch")
    if dispatch is not None and (not isinstance(dispatch, dict)
            or set(dispatch) != {"shift", "max_probe"}
            or type(dispatch["shift"]) is not int or not 0 <= dispatch["shift"] <= 8
            or type(dispatch["max_probe"]) is not int or not 0 <= dispatch["max_probe"] <= 256):
        raise ValueError("%s: translate.dispatch requires shift (0..8) and max_probe (0..256)" % source)
    input_registers = translate.setdefault("private_input_registers", {})
    if not isinstance(input_registers, dict):
        raise ValueError("%s: [translate] private_input_registers must be an address-to-register table" % source)
    parsed_input_registers = {}
    for address, register in input_registers.items():
        if not isinstance(address, str) or not re.fullmatch(r"[0-9a-fA-F]{8}", address):
            raise ValueError("%s: [translate] private_input_registers keys must be 8-digit hexadecimal addresses" % source)
        addr = int(address, 16)
        if addr in parsed_input_registers:
            raise ValueError("%s: [translate] private_input_registers repeats address %08x" % (source, addr))
        if (not isinstance(register, list) or not register or len(set(register)) != len(register)
                or any(not isinstance(name, str) or name not in PRIVATE_INPUT_REGISTERS for name in register)):
            raise ValueError("%s: [translate] private_input_registers values must be lists of distinct %s"
                             % (source, ", ".join(sorted(PRIVATE_INPUT_REGISTERS))))
        parsed_input_registers[addr] = register
    translate["private_input_registers"] = parsed_input_registers
    if type(translate.setdefault("x87_imports_preserve_pc_rc", False)) is not bool:
        raise ValueError("%s: x87_imports_preserve_pc_rc must be a boolean" % source)
    if translate.get("x87_facts") is not None and (not isinstance(translate["x87_facts"], str) or not translate["x87_facts"]):
        raise ValueError("%s: x87_facts must name a compiler fact file" % source)
    if translate.get("analysis_facts") is not None and (not isinstance(translate["analysis_facts"], str) or not translate["analysis_facts"]):
        raise ValueError("%s: analysis_facts must name an analysis fact file" % source)
    if translate.get("indirect_targets") is not None and (not isinstance(translate["indirect_targets"], str) or not translate["indirect_targets"]):
        raise ValueError("%s: indirect_targets must name an indirect target file" % source)
    if translate.get("analysis_facts") is not None and (translate.get("indirect_targets") is not None
            or translate.get("call_effects")):
        raise ValueError("%s: analysis_facts cannot be combined with indirect_targets or call_effects" % source)
    if translate.get("discovery_evidence") is not None and (not isinstance(translate["discovery_evidence"], str) or not translate["discovery_evidence"]):
        raise ValueError("%s: discovery_evidence must name a RECOMP_DISCOVERY file" % source)
    effects = translate.setdefault("call_effects", [])
    if not isinstance(effects, list):
        raise ValueError("%s: call_effects must be an array of tables" % source)
    seen_effects = set()
    for index, row in enumerate(effects):
        where = "%s: call_effects entry %d" % (source, index)
        if not isinstance(row, dict) or not set(row) <= {"import", "site", "regs", "x87"}:
            raise ValueError("%s has unknown keys" % where)
        if ("import" in row) == ("site" in row):
            raise ValueError("%s must set exactly one of import or site" % where)
        if "import" in row:
            dll, sep, name = row["import"].partition("!") if isinstance(row["import"], str) else ("", "", "")
            if not sep or not dll or not name or "!" in name:
                raise ValueError("%s: import must be 'dll!name'" % where)
            row["import"] = dll.lower() + "!" + name
            target = ("import", row["import"])
        else:
            if type(row["site"]) is not int or not 0 <= row["site"] <= 0xffffffff:
                raise ValueError("%s: site must be a 32-bit address" % where)
            target = ("site", row["site"])
        if "regs" not in row and "x87" not in row:
            raise ValueError("%s must set regs or x87" % where)
        if "regs" in row and row["regs"] not in ("abi", "unknown"):
            raise ValueError("%s: regs must be 'abi' or 'unknown'" % where)
        if "x87" in row:
            x87 = row["x87"]
            if isinstance(x87, dict):
                valid = (set(x87) == {"preserved", "known", "value"}
                         and all(type(x87[k]) is int and 0 <= x87[k] <= 0xffff for k in x87)
                         and not x87["value"] & ~x87["known"]
                         and not x87["known"] & x87["preserved"])
            else:
                valid = x87 in ("preserve", "unknown")
            if not valid:
                raise ValueError("%s: x87 must be 'preserve', 'unknown' or {preserved, known, value}" % where)
        if target in seen_effects:
            raise ValueError("%s repeats %s" % (where, target[1]))
        seen_effects.add(target)
    contracts = translate.setdefault("x87_entry_contracts", [])
    if not isinstance(contracts, list):
        raise ValueError("%s: x87_entry_contracts must be a list" % source)
    seen = set()
    for row in contracts:
        addr, mask, value = (row.get(k) for k in ("function", "mask", "value"))
        if (type(addr) is not int or not 0 <= addr <= 0xffffffff or addr in seen
                or type(mask) is not int or not 0 < mask <= 0xffff
                or type(value) is not int or value & ~mask
                or not row.get("evidence") or row.get("scope") != "all_invocations"
                or type(row.get("verify", False)) is not bool):
            raise ValueError("%s: invalid x87 entry contract %r" % (source, row))
        seen.add(addr)
    allocators = translate.setdefault("allocators", [])
    if not isinstance(allocators, list):
        raise ValueError("%s: [translate] allocators must be a list" % source)
    seen_allocators = set()
    for row in allocators:
        addr = row.get("function") if isinstance(row, dict) else None
        if (type(addr) is not int or not 0 <= addr <= 0xffffffff or addr in seen_allocators
                or not isinstance(row.get("evidence"), str) or not row["evidence"]):
            raise ValueError("%s: invalid allocator entry %r" % (source, row))
        seen_allocators.add(addr)
    callers = translate.setdefault("state_contract_callers", [])
    if not isinstance(callers, list) or not all(type(v) is int and 0 <= v <= 0xffffffff for v in callers):
        raise ValueError("%s: [translate] state_contract_callers must be a list of 32-bit addresses" % source)
    alts = translate.setdefault("alternate_entries", [])
    if not isinstance(alts, list) or not all(type(v) is int for v in alts):
        raise ValueError("%s: [translate] alternate_entries must be a list of addresses" % source)
    tracks = cfg.setdefault("media", {}).setdefault("cd_tracks", [])
    if not isinstance(tracks, list) or not all(isinstance(v, str) for v in tracks):
        raise ValueError("%s: [media] cd_tracks must be a list of strings" % source)
    if not isinstance(cfg.setdefault("audio", {}).setdefault("midi", True), bool):
        raise ValueError("%s: [audio] midi must be a boolean" % source)
    cfg.setdefault("hooks", {})
    render = cfg.setdefault("render", {})
    fonts = cfg.setdefault("fonts", {}).setdefault("files", [])
    if not isinstance(fonts, list) or any(
            not isinstance(path, str) or not path or "\0" in path for path in fonts):
        raise ValueError("%s: [fonts] files must be a list of nonempty paths" % source)
    if not isinstance(render.setdefault("d3d8_wgpu", False), bool):
        raise ValueError("%s: [render] d3d8_wgpu must be a boolean" % source)
    if "d3d9_wgpu" in render:
        raise ValueError("%s: [render] d3d9_wgpu was removed; the Rust D3D9 renderer is always used" % source)
    input_config = cfg.setdefault("input", {})
    relative_capture = input_config.setdefault("relative_mouse_capture", True)
    if not isinstance(relative_capture, bool):
        raise ValueError("%s: [input] relative_mouse_capture must be a boolean" % source)
    keyboard_names = input_config.setdefault("keyboard_names", "system")
    if keyboard_names not in ("system", "us"):
        raise ValueError('%s: [input] keyboard_names must be "system" or "us", not %r'
                         % (source, keyboard_names))
    sensitivity = input_config.setdefault("mouse_sensitivity", 100)
    if type(sensitivity) is not int or not 25 <= sensitivity <= 400 or sensitivity % 25:
        raise ValueError("%s: [input] mouse_sensitivity must be an integer from 25 to 400 in steps of 25" % source)
    acceleration = input_config.setdefault("mouse_acceleration", False)
    if not isinstance(acceleration, bool):
        raise ValueError("%s: [input] mouse_acceleration must be a boolean" % source)
    input_config["keyboard_names"] = keyboard_names
    input_config["mouse_sensitivity"] = sensitivity
    input_config["mouse_acceleration"] = acceleration
    cfg.setdefault("bundle", {}).setdefault("exclude", [])
    cfg["controls"] = load_controls(cfg.get("controls", {}), source)
    settings = cfg.setdefault("settings", {})
    window_mode = settings.setdefault("window_mode", "windowed")
    if window_mode not in ("windowed", "borderless", "fullscreen"):
        raise ValueError("%s: [settings] window_mode must be windowed, borderless or fullscreen" % source)
    overlay = settings.setdefault("performance_overlay", "off")
    if overlay not in ("off", "counters", "graph"):
        raise ValueError("%s: [settings] performance_overlay must be off, counters or graph" % source)
    presentation_filter = settings.setdefault("presentation_filter", "bilinear")
    if presentation_filter not in ("nearest", "bilinear"):
        raise ValueError("%s: [settings] presentation_filter must be nearest or bilinear" % source)
    rows = settings.setdefault("rows", list(SETTINGS_ROWS))
    if not isinstance(rows, list):
        raise ValueError("%s: [settings] rows must be a list, not %r" % (source, rows))
    unknown = [row for row in rows if row not in SETTINGS_ROWS]
    if unknown:
        raise ValueError("%s: [settings] rows may name only %s, not %s"
                         % (source, ", ".join(SETTINGS_ROWS), ", ".join(map(repr, unknown))))
    settings["rows"] = rows
    launcher = cfg.setdefault("launcher", {})
    launcher.setdefault("title", game["name"])
    launcher.setdefault("store", "")
    for key in ("install_names", "gog_ids", "steam_ids"):
        value = launcher.setdefault(key, [game["name"]] if key == "install_names" else [])
        if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
            raise ValueError("%s: [launcher] %s must be a list of strings" % (source, key))
    for key in ("title", "store"):
        if not isinstance(launcher[key], str):
            raise ValueError("%s: [launcher] %s must be a string" % (source, key))
    min_free = launcher.setdefault("min_free_mb", 0)
    if not isinstance(min_free, int) or min_free < 0:
        raise ValueError("%s: [launcher] min_free_mb must be a non-negative integer" % source)
    setup_dirs = cfg.setdefault("setup", {}).setdefault("required_dirs", [])
    if not isinstance(setup_dirs, list):
        raise ValueError("%s: [setup] required_dirs must be a list" % source)
    curated = {}
    globals_path = game_dir / translate.get("globals", "globals.toml")
    if globals_path.is_file():
        with globals_path.open("rb") as fh:
            curated = tomllib.load(fh)
    cfg["globals"] = {**curated.get("globals", {}), **cfg.get("globals", {})}
    cfg["curated"] = {key: value for key, value in curated.items() if key != "globals"}
    cfg["dir"] = game_dir
    cfg["source"] = str(source)
    cfg["build_root"] = build_root
    exe = game.get("developer_exe")
    cfg["developer_exe_path"] = ((game_dir / exe) if exe else build_root / "original" / game["executable"]).resolve()
    selected_executable = os.environ.get("RECOMP_SOURCE_EXECUTABLE")
    cfg["source_executable_override"] = bool(selected_executable)
    if selected_executable:
        selected_executable = relative_game_path(selected_executable, source,
                                                  "RECOMP_SOURCE_EXECUTABLE")
        if game.get("developer_exe"):
            raise ValueError("RECOMP_SOURCE_EXECUTABLE cannot override a fixed developer executable")
        cfg["developer_exe_path"] = (build_root / "original" / selected_executable).resolve()
    code_map = translate.get("code_map")
    if code_map is not None and (not isinstance(code_map, str) or not code_map):
        raise ValueError("%s: [translate] code_map must be a non-empty path" % source)
    cfg["code_map_path"] = (game_dir / code_map).resolve() if code_map else None
    cfg["reloc"] = load_reloc(cfg, game_dir, build_root, source)
    cfg["aux_modules"] = load_aux_modules(cfg, game_dir, source)
    lift_heap_above_aux(cfg, source)
    validate_installation_hints(cfg, source)
    # [translate] overrides: a header the generated sources include before they
    # define FN_<addr>, so a game can replace one translated function with a
    # native one (output.py's RECOMP_OVERRIDE_HEADER). Absent by default,
    # and required to exist when named: a path that silently does not resolve
    # would leave the build looking replaced while running the original.
    overrides = translate.get("overrides")
    cfg["overrides_header"] = None
    if overrides is not None:
        path = (game_dir / overrides).resolve()
        if not path.is_file():
            raise ValueError("%s: [translate] overrides names no file: %s" % (source, path))
        cfg["overrides_header"] = path
    validate_guest_layout(cfg, source)
    return cfg



def load_reloc(cfg, game_dir, build_root, source):
    value = cfg.get("reloc")
    if value is None:
        return None
    if not isinstance(value, dict) or set(value) - {"extractor", "args"}:
        raise ValueError("%s: [reloc] must be a table with extractor and optional args" % source)
    extractor = value.get("extractor")
    if not isinstance(extractor, str) or not extractor or "\0" in extractor:
        raise ValueError("%s: [reloc] extractor must be a path relative to the game directory" % source)
    path = Path(extractor.replace("\\", "/"))
    if path.is_absolute():
        raise ValueError("%s: [reloc] extractor must be relative to the game directory" % source)
    path = (game_dir / path).resolve()
    if not path.is_file():
        raise ValueError("%s: [reloc] extractor names no file: %s" % (source, path))
    args = value.get("args", [])
    if not isinstance(args, list) or not all(isinstance(arg, str) for arg in args):
        raise ValueError("%s: [reloc] args must be a list of strings" % source)
    return {"extractor": path, "args": args,
            "manifest_path": Path(build_root) / "recomp/reloc/manifest.json"}

def load_aux_modules(cfg, game_dir, source):
    modules = []
    try:
        source_aux_paths = json.loads(os.environ.get("RECOMP_SOURCE_AUX_PATHS", "{}"))
    except json.JSONDecodeError as error:
        raise ValueError("RECOMP_SOURCE_AUX_PATHS must be a JSON object") from error
    if not isinstance(source_aux_paths, dict) or any(not isinstance(key, str) or not isinstance(value, str)
                                                      for key, value in source_aux_paths.items()):
        raise ValueError("RECOMP_SOURCE_AUX_PATHS must map module names to relative paths")
    guest_size = cfg["game"].get("guest_size", GUEST_SIZE_DEFAULT)
    seen_classes = set()
    for key, entry in sorted(cfg.get("modules", {}).get("aux", {}).items()):
        missing = [name for name in AUX_REQUIRED_KEYS if name not in entry]
        if missing:
            raise ValueError("%s: [modules.aux.%s] missing %s" % (source, key, ", ".join(missing)))
        base, size = entry["base"], entry["size"]
        if base % 0x1000 or size <= 0 or base + size > guest_size:
            raise ValueError("%s: [modules.aux.%s] invalid base or size" % (source, key))
        if "path" in entry and "original_path" in entry:
            raise ValueError("%s: [modules.aux.%s] path and original_path conflict" % (source, key))
        if "path" in entry:
            path = game_dir / entry["path"]
        elif entry["name"] in source_aux_paths:
            relative = relative_game_path(source_aux_paths[entry["name"]], source,
                                          "RECOMP_SOURCE_AUX_PATHS." + entry["name"])
            path = cfg["build_root"] / "original" / relative
        elif cfg.get("source_executable_override"):
            path = cfg["developer_exe_path"].parent / entry.get("original_path", entry["name"])
        else:
            path = cfg["build_root"] / "original" / entry.get("original_path", entry["name"])
        com_classes = []
        for item in entry.get("com_classes", []):
            where = "[modules.aux.%s] com_classes" % key
            if not isinstance(item, dict) or "clsid" not in item or "name" not in item:
                raise ValueError("%s must list tables with clsid and name, not %r" % (where, item))
            clsid = parse_guid(item["clsid"], source, where + ".clsid")
            if clsid in seen_classes:
                raise ValueError("%s repeats a CLSID already registered" % where)
            seen_classes.add(clsid)
            name = item["name"]
            if not isinstance(name, str) or not name:
                raise ValueError("%s.name must be a non-empty string" % where)
            com_classes.append({"clsid": clsid, "name": name})
        aliases = entry.get("aliases", [])
        if not isinstance(aliases, list) or not all(isinstance(a, str) and a for a in aliases):
            raise ValueError("%s: [modules.aux.%s] aliases must be a list of non-empty strings" % (source, key))
        modules.append({"key": key, "name": entry["name"], "aliases": aliases, "sha256": entry["sha256"],
                        "base": base, "size": size, "path": path.resolve(),
                        "code_map_path": (game_dir / entry.get("code_map", "metadata-" + key)).resolve(),
                        "entry_points": entry.get("entry_points", []),
                        "com_classes": com_classes})
    claimed = set()
    for module in modules:
        for name in [module["name"]] + module["aliases"]:
            if name.lower() in claimed:
                raise ValueError("%s: [modules.aux.%s] name or alias %s is already taken" % (source, module["key"], name))
            claimed.add(name.lower())
    ordered = sorted(modules, key=lambda module: module["base"])
    for left, right in zip(ordered, ordered[1:]):
        if left["base"] + left["size"] > right["base"]:
            raise ValueError("%s: auxiliary modules overlap" % source)
    return modules


def lift_heap_above_aux(cfg, source):
    """Start the heap after any fixed auxiliary mapping that lands inside it.

    The configured heap base is the lowest address the heap may start at;
    auxiliary DLLs are placed first and can push it up without the game
    profile having to spell out every layout change."""
    game = cfg["game"]
    base, end = game["heap_base"], game["heap_end"]
    overlapping = [module for module in cfg["aux_modules"]
                   if module["base"] < end and module["base"] + module["size"] > base]
    if not overlapping:
        return
    base = max((module["base"] + module["size"] + 0xfff) & ~0xfff for module in overlapping)
    if base >= end:
        raise ValueError("%s: auxiliary modules leave no room for the heap below %#x"
                         % (source, end))
    game["heap_base"] = base


def validate_guest_layout(cfg, source):
    game = cfg["game"]
    guest_size = int(game.setdefault("guest_size", GUEST_SIZE_DEFAULT))
    if guest_size % 0x1000 or guest_size < GUEST_SIZE_DEFAULT or guest_size > 0xfffff000:
        raise ValueError("%s: [game] guest_size %#x must be page aligned and between %#x and 0xfffff000"
                         % (source, guest_size, GUEST_SIZE_DEFAULT))
    game["guest_size"] = guest_size
    host_base = int(game.setdefault("host_arena_base", 0x300000000))
    if host_base and (host_base % 0x10000 or host_base < 0x100000000
                      or host_base + guest_size > 0x0000800000000000):
        raise ValueError("%s: [game] host_arena_base must be zero for dynamic allocation or a 64 KiB aligned 64-bit user address" % source)
    game["host_arena_base"] = host_base
    heap_base, heap_end = game["heap_base"], game["heap_end"]
    if heap_end % 0x1000 or heap_end > guest_size:
        raise ValueError("%s: [game] heap_end must be page aligned and fit inside guest_size" % source)
    # The mod heap, stack, callback sentinel, TEB and imports retain their
    # fixed low addresses. A larger heap must lie wholly above this band.
    if heap_base < GUEST_SIZE_DEFAULT and heap_end > HEAP_END:
        raise ValueError("%s: [game] heap overlaps reserved runtime memory [%#x, %#x)"
                         % (source, HEAP_END, GUEST_SIZE_DEFAULT))
