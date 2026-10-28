import hashlib
import json
import os
import re
from collections import deque
from pathlib import Path, PurePosixPath
import struct
import unicodedata


SCHEMA = {"major": 1, "minor": 0}
MAX_DEPTH = 16
MAX_ENTRIES = 20000
MAX_FOLDER_ENTRIES = 250000
MAX_CANDIDATES = 64
MAX_HASH_BYTES = 512 * 1024 * 1024
HASH_CHUNK = 1024 * 1024
MAX_PROBE_BYTES = 4 * 1024 * 1024
MAX_SIBLING_ENTRIES = 4096
MAX_SIBLINGS = 64
MAX_DISCOVERY_ROOTS = 128
MAX_DISCOVERY_ENTRIES = 60000
MAX_DISCOVERY_CANDIDATES = 128
MAX_DISCOVERY_HASH_BYTES = 1024 * 1024 * 1024
SIBLING_SUFFIXES = {".bin", ".cab", ".dat", ".iso", ".cue", ".zip", ".exe", ".msi", ".pak", ".001"}
INNO_MAGICS = {
    b"rDlPtS02\x87eVx", b"rDlPtS04\x87eVx", b"rDlPtS05\x87eVx",
    b"rDlPtS06\x87eVx", b"rDlPtS07\x87eVx", b"rDlPtS\xcd\xe6\xd7{\x0b*",
    b"nS5W7dT\x83\xaa\x1b\x0fj",
}


def _fold(value):
    return unicodedata.normalize("NFC", value).casefold()


def _sort_key(value):
    normalized = unicodedata.normalize("NFC", value)
    return normalized.casefold(), normalized, value


def _relative(value):
    return "/".join(PurePosixPath(value.replace("\\", "/")).parts)


def _cancelled(cancel):
    if cancel is not None:
        cancel()


def _diag(code, path, message, details=None, recovery=None):
    return {"code": code, "stage": "inspecting", "path": path, "message": message,
            "details": details or {}, "recovery": recovery or []}


def sibling_payload_facts(source, cancel=None):
    source = Path(source)
    facts = []
    diagnostics = []
    entries = []
    seen = 0
    try:
        with os.scandir(source.parent) as iterator:
            for entry in iterator:
                _cancelled(cancel)
                seen += 1
                if seen > MAX_SIBLING_ENTRIES:
                    diagnostics.append(_diag("sibling_limit", source.name,
                                             "The sibling file limit was reached.",
                                             {"maxSiblingEntries": MAX_SIBLING_ENTRIES},
                                             ["Choose a source folder with fewer sibling files."]))
                    break
                if Path(entry.name).suffix.casefold() in SIBLING_SUFFIXES:
                    entries.append(entry)
    except OSError as error:
        diagnostics.append(_diag("sibling_inaccessible", source.name,
                                 "Sibling source files could not be listed.", {"error": str(error)},
                                 ["Check the selected source folder permissions."]))
        return facts, diagnostics
    entries.sort(key=lambda entry: _sort_key(entry.name))
    groups = {}
    for entry in entries:
        groups.setdefault(_fold(entry.name), []).append(entry.name)
    for names in groups.values():
        if len(names) > 1:
            diagnostics.append(_diag("sibling_collision", source.name,
                                     "Sibling source names collide after Unicode normalization and case folding.",
                                     {"names": sorted(names, key=_sort_key)},
                                     ["Resolve the sibling file name collision before selecting this source."]))
    for entry in entries[:MAX_SIBLINGS]:
        _cancelled(cancel)
        try:
            if entry.is_symlink():
                diagnostics.append(_diag("sibling_symlink", entry.name,
                                         "A symbolic-link sibling was skipped.",
                                         recovery=["Choose source files without symbolic links."]))
                continue
            if not entry.is_file(follow_symlinks=False):
                continue
            info = entry.stat(follow_symlinks=False)
            facts.append({"name": entry.name, "extensionHint": Path(entry.name).suffix.casefold().lstrip("."),
                          "size": info.st_size, "metadataOnly": True})
        except OSError as error:
            diagnostics.append(_diag("sibling_inaccessible", entry.name,
                                     "A sibling source file could not be inspected.", {"error": str(error)},
                                     ["Check the source file permissions."]))
    if len(entries) > MAX_SIBLINGS:
        diagnostics.append(_diag("sibling_limit", source.name,
                                 "The related-file limit was reached.", {"maxSiblings": MAX_SIBLINGS},
                                 ["Choose a source folder with fewer related files."]))
    return facts, diagnostics


def _safe_path(root, path):
    root = root.resolve(strict=True)
    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise OSError("path escapes selected source") from error
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise OSError("symbolic link encountered")
    resolved = path.resolve(strict=True)
    try:
        resolved.relative_to(root)
    except ValueError as error:
        raise OSError("resolved path escapes selected source") from error
    return resolved


def _digest(path, root, cancel, byte_budget):
    count = 0
    try:
        canonical = _safe_path(root, path)
        before = path.stat()
        if not path.is_file():
            raise OSError("not a regular file")
        hash_limit = byte_budget[1] if len(byte_budget) > 1 else MAX_HASH_BYTES
        if before.st_size > hash_limit or byte_budget[0] + before.st_size > hash_limit:
            return {"status": "limit", "size": before.st_size, "sha256": None}
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            opened = os.fstat(stream.fileno())
            initial = (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
            if (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns) != initial:
                return {"status": "changed", "size": before.st_size, "sha256": None}
            while True:
                _cancelled(cancel)
                remaining = before.st_size - count
                if remaining <= 0:
                    break
                chunk = stream.read(min(HASH_CHUNK, remaining))
                if not chunk:
                    break
                count += len(chunk)
                byte_budget[0] += len(chunk)
                digest.update(chunk)
        after = path.stat()
        if count != before.st_size or initial != (
                after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
            return {"status": "changed", "size": count, "sha256": None}
        if _safe_path(root, path) != canonical:
            return {"status": "changed", "size": count, "sha256": None}
        return {"status": "verified", "size": count, "sha256": digest.hexdigest()}
    except (OSError, ValueError) as error:
        return {"status": "unreadable", "size": count, "sha256": None, "error": str(error)}


def _scan(root, cancel, diagnostics, discovery_budget=None):
    queue = deque([(root, 0)])
    files = []
    directory_index = {}
    entries_seen = 0
    root_stat = root.stat()
    visited_dirs = {(root_stat.st_dev, root_stat.st_ino)}
    while queue:
        _cancelled(cancel)
        directory, depth = queue.popleft()
        try:
            _safe_path(root, directory)
        except OSError as error:
            diagnostics.append(_diag("unsafe_path", directory.relative_to(root).as_posix() or ".",
                                     "A folder escaped the selected source or changed during inspection.",
                                     {"error": str(error)}, ["Choose a source without symbolic links."]))
            continue
        entries = []
        overflow = False
        try:
            with os.scandir(directory) as iterator:
                for entry in iterator:
                    _cancelled(cancel)
                    entries_seen += 1
                    if (entries_seen > MAX_FOLDER_ENTRIES or
                            discovery_budget is not None and discovery_budget[0] >= MAX_DISCOVERY_ENTRIES):
                        overflow = True
                        break
                    if discovery_budget is not None:
                        discovery_budget[0] += 1
                    entries.append(entry)
        except OSError as error:
            diagnostics.append(_diag("inaccessible_path", directory.relative_to(root).as_posix() or ".",
                                     "A folder could not be inspected.", {"error": str(error)},
                                     ["Choose a readable game folder."]))
            continue
        if overflow:
            diagnostics.append(_diag("search_limit", directory.relative_to(root).as_posix() or ".",
                                     "The source entry limit was reached.",
                                     {"maxEntries": MAX_FOLDER_ENTRIES,
                                      "maxDiscoveryEntries": MAX_DISCOVERY_ENTRIES,
                                      "entriesSeen": entries_seen},
                                     ["Choose a smaller source folder."]))
            break
        entries.sort(key=lambda entry: _sort_key(entry.name))
        groups = {}
        for entry in entries:
            groups.setdefault(_fold(entry.name), []).append(entry)
        collided = set()
        for group in groups.values():
            if len(group) > 1:
                collided.update(entry.name for entry in group)
                diagnostics.append(_diag("path_collision", directory.relative_to(root).as_posix() or ".",
                                         "Names collide after Unicode normalization and case folding.",
                                         {"names": sorted((entry.name for entry in group), key=_sort_key)},
                                         ["Resolve the name collision in the source folder."]))
        parent_rel = directory.relative_to(root).as_posix()
        parent_key = _fold(parent_rel) if parent_rel != "." else ""
        directory_index[parent_key] = {}
        for entry in entries:
            if entry.name in collided:
                continue
            rel = Path(entry.path).relative_to(root).as_posix()
            try:
                if entry.is_symlink():
                    directory_index[parent_key][_fold(entry.name)] = {"collision": True}
                    diagnostics.append(_diag("symlink_skipped", rel,
                                             "A symbolic link was skipped during source discovery.",
                                             recovery=["Choose a source without symbolic links."]))
                elif entry.is_file(follow_symlinks=False):
                    record = {"name": entry.name, "kind": "file", "path": Path(entry.path)}
                    directory_index[parent_key][_fold(entry.name)] = record
                    files.append((rel, Path(entry.path)))
                elif entry.is_dir(follow_symlinks=False):
                    record = {"name": entry.name, "kind": "directory", "path": Path(entry.path)}
                    directory_index[parent_key][_fold(entry.name)] = record
                    info = entry.stat(follow_symlinks=False)
                    identity = (info.st_dev, info.st_ino)
                    if identity in visited_dirs:
                        diagnostics.append(_diag("duplicate_directory", rel,
                                                 "A folder with an already-scanned identity was skipped."))
                    elif depth < MAX_DEPTH:
                        visited_dirs.add(identity)
                        queue.append((Path(entry.path), depth + 1))
                    else:
                        diagnostics.append(_diag("search_limit", rel, "The source depth limit was reached.",
                                                 {"maxDepth": MAX_DEPTH},
                                                 ["Choose the game folder rather than a higher-level folder."]))
            except OSError as error:
                diagnostics.append(_diag("inaccessible_path", rel, "An item could not be inspected.",
                                         {"error": str(error)}, ["Check source permissions."]))
    return files, entries_seen, directory_index


def _content_root(executable_relative, executable_dir):
    executable_parts = PurePosixPath(executable_relative).parts
    parent_parts = executable_parts[:-1]
    expected = tuple(part for part in _relative(executable_dir).split("/") if part not in ("", "."))
    if not expected:
        return "/".join(parent_parts), True
    if len(parent_parts) >= len(expected) and tuple(_fold(p) for p in parent_parts[-len(expected):]) == tuple(
            _fold(p) for p in expected):
        return "/".join(parent_parts[:-len(expected)]), True
    return "/".join(parent_parts), False


def _resolve_case(root, base_relative, relative, want_directory, directory_index, cancel):
    parts = tuple(part for part in PurePosixPath(relative.replace("\\", "/")).parts if part not in ("", "."))
    base_parts = tuple(part for part in PurePosixPath(base_relative).parts if part not in ("", "."))
    actual = list(base_parts)
    for index, part in enumerate(parts):
        _cancelled(cancel)
        parent_key = _fold("/".join(actual)) if actual else ""
        entry = directory_index.get(parent_key, {}).get(_fold(part))
        if not entry or entry.get("collision"):
            return None, "ambiguous_or_missing_path" if entry else None
        is_last = index == len(parts) - 1
        expected_kind = "directory" if (want_directory or not is_last) else "file"
        if entry["kind"] != expected_kind:
            return None, None
        if not entry.get("listed"):
            try:
                _safe_path(root, entry["path"])
            except OSError:
                return None, "unsafe_path"
        actual.append(entry["name"])
    return ("/".join(actual[len(base_parts):]) if actual[len(base_parts):] else "."), None


def validate_required_roles(cfg, root, cancel=None, directory_index=None, byte_budget=None,
                            base_relative="", executable_relative=None, executable_fact=None,
                            artifacts=None, listed=False):
    root = Path(root).resolve(strict=True)
    if directory_index is None:
        diagnostics = []
        _, _, directory_index = _scan(root, cancel, diagnostics)
    else:
        diagnostics = []
    budget = byte_budget if byte_budget is not None else [0, MAX_HASH_BYTES]
    checks = []
    missing_required = False
    modules_ok = True
    for artifact in artifacts if artifacts is not None else cfg["setup"]["required_roles"]:
        role = artifact["role"]
        if role == "executable" and executable_relative is not None:
            actual, reason, fact = executable_relative, None, executable_fact
        else:
            actual, reason = _resolve_case(root, base_relative, artifact["path"],
                                           artifact["kind"] == "directory", directory_index, cancel)
            fact = None
            if actual is not None and artifact["kind"] == "file":
                path = root / Path(*PurePosixPath(base_relative).parts) / Path(*PurePosixPath(actual).parts)
                fact = _digest(path, root, cancel, budget) if artifact.get("sha256") or role == "executable" else None
        expected_hash = cfg["game"]["sha256"].lower() if role == "executable" else artifact.get("sha256")
        if role == "executable":
            if fact is None:
                fact = {"status": "missing" if actual is None else "unreadable", "sha256": None}
            found = fact["status"] == "verified" and fact.get("sha256") == expected_hash
            reason = "wrong_version" if fact["status"] == "verified" and not found else reason
        elif fact is not None:
            found = fact["status"] == "verified" and fact.get("sha256") == expected_hash.lower()
        else:
            found = actual is not None
        if fact is not None:
            artifact_status = ("wrong_version" if fact["status"] == "verified" and not found else fact["status"])
        else:
            artifact_status = ("listed" if listed else "verified") if found else "missing" if reason is None else "unreadable"
        checks.append({"id": artifact.get("id"), "role": role, "path": artifact["path"], "kind": artifact["kind"],
                       "required": artifact.get("required", True), "found": found, "status": artifact_status,
                       "expectedSha256": expected_hash, "actualPath": actual, "reason": reason,
                       "fingerprint": fact})
        if not found and artifact.get("required", True):
            missing_required = True
            if role.startswith("auxiliary:"):
                modules_ok = False
            diagnostics.append(_diag("required_artifact_" + artifact_status,
                                     actual or artifact["path"],
                                     "A required game file or folder could not be verified.",
                                     {"role": role, "expectedSha256": expected_hash},
                                     ["Supply the exact supported game file at the listed relative path."]))
    return checks, diagnostics, missing_required, modules_ok


def _candidate(cfg, root, selected_relative, cancel, byte_budget, diagnostics, directory_index, listed=False):
    game = cfg["game"]
    setup = cfg["setup"]
    layout = setup["layout"]
    main = root / Path(*PurePosixPath(selected_relative).parts)
    main_fact = _digest(main, root, cancel, byte_budget)
    expected_main = game["sha256"].lower()
    matched_version = None
    if main_fact["status"] == "verified" and main_fact["sha256"] != expected_main:
        for version_id, version in setup["versions"].items():
            if (version["sha256"].lower() == main_fact["sha256"]
                    and version["size"] == main_fact["size"]):
                matched_version = (version_id, version)
                break

    content_relative, layout_matches = _content_root(selected_relative, layout["executable_dir"])
    content_root = root / Path(*PurePosixPath(content_relative).parts) if content_relative else root
    try:
        _safe_path(root, content_root)
    except OSError:
        return None
    actual_exec_rel = selected_relative[len(content_relative) + 1:] if content_relative else selected_relative
    main_supported = main_fact["status"] == "verified" and main_fact["sha256"] == expected_main
    checks, role_diagnostics, missing_required, modules_ok = validate_required_roles(
        cfg, root, cancel, directory_index, byte_budget, content_relative,
        actual_exec_rel, main_fact, listed=listed)
    diagnostics.extend(role_diagnostics)

    identity_status = "unknown"
    label = None
    preparations = []
    if main_supported:
        identity_status = "supported"
        label = "Supported version"
    elif matched_version:
        _, version = matched_version
        identity_status = version["status"]
        label = version["label"]
        preparations = list(version["preparations"])
    if identity_status == "unknown" and main_fact["status"] == "verified":
        diagnostics.append(_diag("unknown_executable_version", selected_relative,
                                 "The executable does not match a recognized game version.",
                                 {"size": main_fact["size"], "sha256": main_fact["sha256"]},
                                 ["Choose an installed folder with a supported game version."]))
    status = identity_status
    if identity_status == "supported" and (missing_required or not layout_matches):
        status = "incomplete"
    data_root, data_root_error = _resolve_case(root, content_relative, layout["data_root"], True,
                                              directory_index, cancel)
    data_root_found = data_root is not None if layout["data_root"] != "." else True
    if not data_root_found and identity_status == "supported":
        status = "incomplete"
    canonical = _fold(PurePosixPath(actual_exec_rel).name) == _fold(game["executable"])
    installable = (main_supported and not missing_required and modules_ok and data_root_found
                   and layout_matches and canonical)
    if not layout_matches:
        diagnostics.append(_diag("layout_mismatch", selected_relative,
                                 "The executable is outside the configured executable folder layout.",
                                 recovery=["Choose the installed game folder with the configured layout."]))
    if not canonical:
        message = ("The executable name does not match the configured game name."
                   if identity_status == "unknown" else
                   "The executable has a recognized game identity under another name.")
        diagnostics.append(_diag("executable_name_mismatch", selected_relative, message,
                                 {"recognizedVersion": identity_status},
                                 ["Choose the installed game folder with the expected executable name."]))
    if main_fact["status"] == "limit":
        status = "incomplete"
        diagnostics.append(_diag("hash_limit", selected_relative,
                                 "The executable exceeded the inspection hash budget.",
                                 {"maxHashBytes": MAX_HASH_BYTES},
                                 ["Choose a source containing fewer or smaller files."]))
    elif main_fact["status"] in ("changed", "unreadable"):
        status = "incomplete"
        diagnostics.append(_diag("source_changed" if main_fact["status"] == "changed" else "unreadable_file",
                                 selected_relative, "The executable could not be verified during inspection.",
                                 {"error": main_fact.get("error")},
                                 ["Check the source file and try again."]))
    if not data_root_found:
        diagnostics.append(_diag("missing_data_root", layout["data_root"],
                                 data_root_error or "The configured data root is missing or ambiguous.",
                                 recovery=["Choose a complete installed game folder."]))
    candidate_id = hashlib.sha256((str(content_root.resolve()) + "\0" + actual_exec_rel + "\0"
                                   + str(main_fact.get("sha256"))).encode("utf-8")).hexdigest()[:24]
    candidate_diagnostics = list(diagnostics)
    preparation_details = []
    for preparation_id in preparations:
        definition = setup.get("preparations", {}).get(preparation_id, {})
        preparation_details.append({"id": preparation_id,
                                    "mode": definition.get("mode", "manual"),
                                    "explanation": definition.get("explanation", ""),
                                    "requiredInputs": definition.get("required_inputs", []),
                                    "requiredTools": definition.get("required_tools", []),
                                    "outputs": definition.get("outputs", [])})
    actual_data_root = data_root or "."
    return {"id": candidate_id, "contentRoot": str(content_root), "contentRootRelative": content_relative or ".",
            "executable": {"path": actual_exec_rel, "size": main_fact.get("size"),
                           "sha256": main_fact.get("sha256"), "status": main_fact["status"]},
            "dataRoot": actual_data_root,
            "layout": {"executableDir": layout["executable_dir"], "dataRoot": layout["data_root"]},
            "version": {"status": identity_status, "label": label, "preparations": preparations,
                        "preparationDetails": preparation_details},
            "status": status, "installable": installable, "requiredRoles": checks,
            "diagnostics": candidate_diagnostics}


def inspect_folder(cfg, selected_path, cancel=None, selected_executable=None, discovery_budget=None,
                   archive_entries=None, include_installers=False):
    selected = Path(selected_path).expanduser().resolve(strict=True)
    expected = tuple(part for part in _relative(cfg["setup"]["layout"]["executable_dir"]).split("/")
                     if part not in ("", "."))
    if (selected.is_dir() and expected and len(selected.parts) >= len(expected)
            and tuple(_fold(part) for part in selected.parts[-len(expected):]) == tuple(_fold(part) for part in expected)
            and (selected / cfg["game"]["executable"]).is_file()):
        selected = (selected / cfg["game"]["executable"]).resolve(strict=True)
    if selected.is_file():
        selected_root = selected.parent
        parent_parts = selected_root.parts
        if expected and len(parent_parts) >= len(expected) and tuple(
                _fold(part) for part in parent_parts[-len(expected):]) == tuple(_fold(part) for part in expected):
            for _ in expected:
                selected_root = selected_root.parent
        selected_executable = selected
    elif selected.is_dir():
        selected_root = selected
    else:
        raise ValueError("Selected source is not a regular file or folder")
    diagnostics = []
    try:
        if archive_entries is None:
            files, entries_seen, directory_index = _scan(selected_root, cancel, diagnostics, discovery_budget)
        else:
            files, directory_index = [], {"": {}}
            entries_seen = len(archive_entries)
            for relative, size in archive_entries.items():
                _cancelled(cancel)
                parts = PurePosixPath(relative).parts
                for index, part in enumerate(parts):
                    parent = "/".join(parts[:index])
                    member = "/".join(parts[:index + 1])
                    kind = "file" if index == len(parts) - 1 and size is not None else "directory"
                    directory_index.setdefault(_fold(parent), {})[_fold(part)] = {
                        "name": part, "kind": kind, "path": selected_root / member, "listed": True}
                    if kind == "directory":
                        directory_index.setdefault(_fold(member), {})
                if size is not None:
                    files.append((relative, selected_root / relative))
    except OSError as error:
        diagnostics.append(_diag("inaccessible_path", ".", "The selected game folder could not be inspected.",
                                 {"error": str(error)}, ["Choose a readable game folder."]))
        files, entries_seen, directory_index = [], 0, {}
    requested_names = {_fold(name) for name in cfg["launcher"]["candidate_executables"]}
    candidates = {}
    for relative, path in files:
        if _fold(relative.rsplit("/", 1)[-1]) in requested_names:
            candidates[relative] = path
    if selected_executable is not None:
        try:
            relative = _relative(selected_executable.resolve(strict=True).relative_to(selected_root).as_posix())
            candidates[relative] = selected_executable
        except ValueError as error:
            raise ValueError("Selected executable is outside its containing source folder") from error
    byte_budget = discovery_budget[1] if discovery_budget is not None else [0, MAX_HASH_BYTES]
    inspected = []
    for relative in sorted(candidates, key=_sort_key):
        _cancelled(cancel)
        if (len(inspected) >= MAX_CANDIDATES or
                discovery_budget is not None and discovery_budget[2] >= MAX_DISCOVERY_CANDIDATES):
            diagnostics.append(_diag("candidate_limit", ".", "The candidate limit was reached.",
                                     {"maxCandidates": MAX_CANDIDATES},
                                     ["Choose a folder with fewer executable candidates."]))
            break
        if discovery_budget is not None:
            discovery_budget[2] += 1
        local_diagnostics = []
        candidate = _candidate(cfg, selected_root, relative, cancel, byte_budget,
                               local_diagnostics, directory_index, listed=archive_entries is not None)
        if candidate:
            inspected.append(candidate)
            for diagnostic in local_diagnostics:
                diagnostic = dict(diagnostic)
                diagnostic["candidateId"] = candidate["id"]
                diagnostics.append(diagnostic)
    source_kind = "executable" if selected_executable is not None else "folder"
    scan_complete = not any(item["code"] in ("search_limit", "candidate_limit", "hash_limit",
                                              "required_artifact_limit", "inaccessible_path",
                                              "path_collision", "symlink_skipped", "unsafe_path",
                                              "sibling_limit", "sibling_collision", "sibling_symlink",
                                              "sibling_inaccessible")
                            for item in diagnostics)
    result = {"schema": SCHEMA, "source": {"kind": source_kind, "path": str(selected),
            "identity": {"payload": "pending",
                         "reason": "Full payload fingerprinting is performed after candidate selection."}},
            "candidates": inspected, "diagnostics": diagnostics,
            "limits": {"maxDepth": MAX_DEPTH, "maxEntries": MAX_FOLDER_ENTRIES,
                       "maxCandidates": MAX_CANDIDATES, "maxHashBytes": byte_budget[1],
                       "entriesSeen": entries_seen, "hashBytes": byte_budget[0]},
            "scanComplete": scan_complete}
    if include_installers and scan_complete and not any(item["installable"] for item in inspected):
        possible = [path for _, path in files if path.suffix.casefold() == ".exe"
                    or path.name.casefold() == "data1.cab"]
        if len(possible) > MAX_CANDIDATES:
            diagnostics.append(_diag("installer_probe_limit", ".", "The folder contains too many possible installers.",
                                     {"maxCandidates": MAX_CANDIDATES},
                                     ["Choose the installer file or a smaller folder."]))
            result["scanComplete"] = False
        else:
            installers = {}
            for path in sorted(possible, key=lambda path: _sort_key(path.as_posix())):
                _cancelled(cancel)
                kind = probe_bytes(path, cancel)["kind"]
                if kind in ("installshield", "installshield_cab"):
                    key = (path.parent, "installshield")
                    if key not in installers or path.name.casefold() == "setup.exe":
                        installers[key] = str(path)
                elif kind in ("inno", "unsupported_inno"):
                    installers[(path, kind)] = str(path)
            result["source"]["installers"] = list(installers.values())
    return result


def _resource_nodes(node):
    for entry in getattr(node, "entries", ()):
        if getattr(entry, "data", None):
            yield entry.data
        elif getattr(entry, "directory", None):
            yield from _resource_nodes(entry.directory)


def _inno_resource(prefix):
    try:
        import pefile
    except ImportError:
        return "unavailable"
    try:
        pe = pefile.PE(data=prefix, fast_load=True)
        resource_index = pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_RESOURCE"]
        pe.parse_data_directories(directories=[resource_index])
        directory = getattr(pe, "DIRECTORY_ENTRY_RESOURCE", None)
        if directory is None:
            return "none"
        for resource_type in getattr(directory, "entries", ()):
            if getattr(resource_type, "id", None) != 10 or not getattr(resource_type, "directory", None):
                continue
            for entry in _resource_nodes_with_id(resource_type.directory):
                data_entries = (_resource_nodes(entry.directory) if getattr(entry, "directory", None)
                                else [entry.data] if getattr(entry, "data", None) else [])
                for data in data_entries:
                    offset = pe.get_offset_from_rva(data.struct.OffsetToData)
                    if offset < 0 or offset + 12 > len(prefix):
                        return "limit"
                    if prefix[offset:offset + 12] in INNO_MAGICS:
                        return "recognized"
                    if prefix[offset:offset + 7] in (b"rDlPtS", b"nS5W7dT"):
                        return "unsupported"
                    return "none"
        return "none"
    except (pefile.PEFormatError, OSError, ValueError, KeyError, AttributeError,
            TypeError, IndexError, struct.error):
        return "unavailable"


def _resource_nodes_with_id(node):
    for entry in getattr(node, "entries", ()):
        name = getattr(entry, "name", None)
        text = getattr(name, "string", None)
        if getattr(entry, "id", None) == 11111 or text in (b"11111", "11111"):
            yield entry
        if getattr(entry, "directory", None):
            yield from _resource_nodes_with_id(entry.directory)


def probe_bytes(path, cancel=None):
    path = Path(path)
    try:
        with path.open("rb") as stream:
            prefix = bytearray()
            while len(prefix) < MAX_PROBE_BYTES:
                _cancelled(cancel)
                chunk = stream.read(min(HASH_CHUNK, MAX_PROBE_BYTES - len(prefix)))
                if not chunk:
                    break
                prefix.extend(chunk)
            prefix = bytes(prefix)
            if prefix.startswith((b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08")):
                return {"kind": "zip", "evidence": "ZIP signature"}
            if prefix.startswith(b"ISc("):
                return {"kind": "installshield_cab", "evidence": "InstallShield Cabinet signature"}
            if prefix.startswith(b"MSCF"):
                return {"kind": "cabinet", "evidence": "Microsoft Cabinet signature"}
            if len(prefix) >= 12 and prefix[:12] == b"\0" + b"\xff" * 10 + b"\0":
                return {"kind": "raw_cd", "evidence": "Raw CD sector sync pattern"}
            if prefix.startswith(b"MZ") and len(prefix) >= 64:
                pe_offset = int.from_bytes(prefix[60:64], "little")
                if pe_offset + 4 <= len(prefix) and prefix[pe_offset:pe_offset + 4] == b"PE\0\0":
                    if len(prefix) >= 0x3C and prefix[0x30:0x34] == b"Inno":
                        offset = struct.unpack_from("<I", prefix, 0x34)[0]
                        inverse = struct.unpack_from("<I", prefix, 0x38)[0]
                        if offset == (~inverse & 0xffffffff):
                            if offset + 12 > len(prefix):
                                return {"kind": "probe_limit", "evidence": "Inno Setup loader target lies outside the bounded probe"}
                            if prefix[offset:offset + 12] in INNO_MAGICS:
                                return {"kind": "inno", "evidence": "Inno Setup loader pointer and recognized signature"}
                            return {"kind": "unsupported_inno", "evidence": "Inno Setup loader pointer has an unrecognized signature"}
                    marker = _inno_resource(prefix)
                    if marker == "recognized":
                        return {"kind": "inno", "evidence": "Inno Setup resource 11111 with recognized loader signature"}
                    if marker == "limit":
                        return {"kind": "probe_limit", "evidence": "Inno Setup resource marker lies outside the bounded header probe"}
                    if marker == "unsupported":
                        return {"kind": "unsupported_inno", "evidence": "Inno Setup resource 11111 has an unrecognized loader signature"}
                    if marker == "unavailable":
                        return {"kind": "probe_limit", "evidence": "PE resource tree could not be verified within the bounded probe"}
                    if path.name.casefold() == "setup.exe":
                        with os.scandir(path.parent) as siblings:
                            for index, sibling in enumerate(siblings):
                                if index >= MAX_SIBLING_ENTRIES:
                                    break
                                if (sibling.name.casefold() == "data1.cab" and not sibling.is_symlink()
                                        and sibling.is_file(follow_symlinks=False)):
                                    with open(sibling.path, "rb") as cabinet:
                                        if cabinet.read(4) == b"ISc(":
                                            return {"kind": "installshield", "evidence": "PE setup executable with adjacent InstallShield data1.cab"}
                    return {"kind": "pe", "evidence": "PE signature"}
                return {"kind": "probe_limit" if len(prefix) == MAX_PROBE_BYTES else "unknown",
                        "evidence": "PE header lies outside the bounded probe"}
            stream.seek(0x8001)
            if stream.read(5) == b"CD001":
                return {"kind": "iso9660", "evidence": "ISO 9660 primary volume descriptor signature"}
            stream.seek(0)
            size = path.stat().st_size
            if size >= 2352 and stream.read(12) == b"\0" + b"\xff" * 10 + b"\0":
                return {"kind": "raw_cd", "evidence": "Raw CD sector sync pattern"}
            cue = prefix[:65536].decode("ascii", errors="ignore").upper().splitlines()
            if any(re.match(r"^\s*FILE\s+.+\s+(BINARY|MOTOROLA|WAVE|MP3)\s*$", line) for line in cue) and any(
                    re.match(r"^\s*TRACK\s+\d+\s+", line) for line in cue):
                return {"kind": "cue", "evidence": "CUE FILE and TRACK records"}
        suffix = path.suffix.casefold()
        if suffix in (".iso", ".bin", ".cue"):
            return {"kind": "media_hint", "evidence": f"{suffix} filename extension only; disc image format is unverified"}
        return {"kind": "unknown", "evidence": "No supported bounded source signature found"}
    except OSError as error:
        return {"kind": "unreadable", "evidence": str(error)}


def discovery_root_rows():
    path = Path(__file__).resolve().parents[1] / "host/launcher/discovery_paths.inc"
    row_pattern = re.compile(r'^RECOMP_DISCOVERY_ROOT\(("(?:\\.|[^"\\])*"), ("(?:\\.|[^"\\])*"), ("(?:\\.|[^"\\])*")\)$')
    rows = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        match = row_pattern.fullmatch(line)
        if not match:
            raise ValueError(f"Invalid shared discovery path row at line {line_number}")
        rows.append(tuple(json.loads(part) for part in match.groups()))
    return rows


def _bounded_child_dirs(path, state, diagnostics, cancel):
    result = []
    overflow = False
    try:
        with os.scandir(path) as iterator:
            for entry in iterator:
                _cancelled(cancel)
                state["entries"] += 1
                if state["entries"] > MAX_DISCOVERY_ENTRIES:
                    overflow = True
                    diagnostics.append(_diag("discovery_entry_limit", str(path),
                                             "The discovery folder-entry limit was reached.",
                                             {"maxDiscoveryEntries": MAX_DISCOVERY_ENTRIES},
                                             ["Choose a narrower search location."]))
                    break
                if not entry.is_symlink() and entry.is_dir(follow_symlinks=False):
                    result.append(Path(entry.path))
    except OSError as error:
        if getattr(error, "errno", None) == 2:
            return result
        diagnostics.append(_diag("discovery_path_unavailable", str(path),
                                 "A configured game-search location could not be read.",
                                 {"error": str(error)}, ["Check the search location permissions."]))
    if overflow:
        return []
    return sorted(result, key=lambda item: _sort_key(item.name))


def expand_discovery_roots(platform_name, home, env=None, cancel=None, install_names=()):
    env = env or os.environ
    home = Path(home) if home else Path()
    rows = discovery_root_rows()
    prioritized_roots = []
    broad_roots = []
    diagnostics = []
    state = {"entries": 0}
    drive_installs = [Path(value) for _, kind, value in rows if kind == "drive_install"]
    wanted_names = {_fold(name) for name in install_names}
    priorities = {"system_drive": 0, "steam": 1, "bottles": 2, "wine": 3,
                  "compatdata": 4, "games_wine": 5, "volume_children": 6,
                  "media_user_children": 7, "home": 8, "absolute": 9}
    active_rows = [row for row in rows if row[0] in (platform_name, "shared")]
    active_rows.sort(key=lambda row: (priorities.get(row[1], 10), rows.index(row)))
    for row_platform, kind, value in active_rows:
        _cancelled(cancel)
        if state["entries"] > MAX_DISCOVERY_ENTRIES:
            break
        if kind == "drive_install":
            continue
        locations = []
        if kind == "home":
            locations = [home / value]
        elif kind == "absolute":
            locations = [Path(value)]
        elif kind == "system_drive":
            drive = env.get("SystemDrive") or "C:"
            locations = [Path(drive + "/") / sub for sub in drive_installs]
        elif kind == "wine":
            locations = [home / value / sub for sub in drive_installs]
        elif kind in ("games_wine", "bottles"):
            parent = home / value
            children = _bounded_child_dirs(parent, state, diagnostics, cancel)
            locations = [child / "drive_c" / sub for child in children for sub in drive_installs]
        elif kind == "steam":
            steam = home / value
            locations = [steam / "steamapps/common"]
            vdf = steam / "steamapps/libraryfolders.vdf"
            try:
                with vdf.open("rb") as stream:
                    contents = stream.read(1024 * 1024 + 1)
                if len(contents) > 1024 * 1024:
                    diagnostics.append(_diag("steam_library_file_limit", str(vdf),
                                             "The Steam library list exceeds the inspection limit.",
                                             {"maxBytes": 1024 * 1024},
                                             ["Reduce the Steam library list before discovery."]))
                else:
                    for line in contents.decode("utf-8", errors="replace").splitlines():
                        _cancelled(cancel)
                        match = re.search(r'"path"\s*"((?:[^"\\]|\\.)*)"', line)
                        if match:
                            if len(locations) >= MAX_DISCOVERY_ROOTS:
                                diagnostics.append(_diag("discovery_root_limit", str(vdf),
                                                         "The Steam library location limit was reached.",
                                                         {"maxDiscoveryRoots": MAX_DISCOVERY_ROOTS},
                                                         ["Choose fewer Steam library locations."]))
                                break
                            library = match.group(1).replace("\\\\", "\\")
                            locations.append(Path(library) / "steamapps/common")
            except OSError as error:
                if getattr(error, "errno", None) != 2:
                    diagnostics.append(_diag("steam_library_file_unavailable", str(vdf),
                                             "The Steam library list could not be read.",
                                             {"error": str(error)},
                                             ["Check the Steam library list permissions."]))
        elif kind == "compatdata":
            compat = home / value / "steamapps/compatdata"
            children = _bounded_child_dirs(compat, state, diagnostics, cancel)
            locations = [child / "pfx/drive_c" / sub for child in children for sub in drive_installs]
        elif kind == "volume_children":
            locations = _bounded_child_dirs(Path(value), state, diagnostics, cancel)
        elif kind == "media_user_children":
            locations = _bounded_child_dirs(Path(value) / env.get("USER", ""), state, diagnostics, cancel)
        else:
            raise ValueError(f"Unknown shared discovery provider: {kind}")
        if wanted_names and kind not in ("volume_children", "media_user_children"):
            for location in locations:
                children = _bounded_child_dirs(location, state, diagnostics, cancel)
                prioritized_roots.extend(child for child in children if _fold(child.name) in wanted_names)
        broad_roots.extend(locations)
    roots = []
    seen_roots = set()
    for location in prioritized_roots + broad_roots:
        key = os.path.normcase(os.path.normpath(str(location)))
        if key in seen_roots:
            continue
        seen_roots.add(key)
        if len(roots) >= MAX_DISCOVERY_ROOTS:
            diagnostics.append(_diag("discovery_root_limit", str(location),
                                     "The configured search-location limit was reached.",
                                     {"maxDiscoveryRoots": MAX_DISCOVERY_ROOTS},
                                     ["Choose a narrower search location."]))
            break
        roots.append(location)
    return roots, diagnostics, state["entries"]
