import argparse
from contextlib import nullcontext
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import plistlib
import queue
import re
import time
import shutil
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import zipfile

import game_config
import source_inspection
from setup import validate_game
from web_launcher import game_entry
from source_inspection import (MAX_DISCOVERY_CANDIDATES, MAX_DISCOVERY_ENTRIES,
                               MAX_DISCOVERY_HASH_BYTES, MAX_DISCOVERY_ROOTS,
                               expand_discovery_roots, inspect_folder, probe_bytes,
                               sibling_payload_facts)
import zip_adapter
from sevenzip import DependencyError, acquire_dependencies
from zip_adapter import ZipError
from inno_adapter import InnoAdapter, InnoError
from unshield_adapter import UnshieldAdapter, library as unshield_library

CANCEL_FILE = None
KIT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(KIT / "tools/recomp"))
import install_jobs


DATA_DIR = "Game Data"


class SourceError(ValueError):
    def __init__(self, code, message, recovery, path=None, details=None):
        super().__init__(message)
        self.diagnostic = {"code": code, "stage": "inspecting", "path": path or ".",
                           "message": message, "details": details or {}, "recovery": recovery}


def emit(kind, **fields):
    print(json.dumps({"kind": kind, **fields}), flush=True)


def file_progress(total=None):
    next_update = 0

    def report(completed, path):
        nonlocal next_update
        now = time.monotonic()
        if now < next_update and completed != total:
            return
        next_update = now + 0.25
        emit("progress", current=path, bytes=completed, totalBytes=total,
             fraction=min(1, completed / total) if total else None)

    return report


def cancel(signum, frame):
    raise InterruptedError("Installation cancelled")


def check_cancel():
    if CANCEL_FILE is not None and CANCEL_FILE.exists():
        raise InterruptedError("Installation cancelled")


DESTINATION_LOCKS = []


class BuildFailure(ValueError):
    def __init__(self, command, returncode, log_path):
        self.command = [str(part) for part in command]
        self.returncode = returncode
        self.log_path = str(log_path) if log_path else None
        super().__init__(f"{Path(command[0]).name} failed with exit code {returncode}")


def run(command, log_path=None, build_monitor=None, env=None):
    check_cancel()
    log = None
    if log_path:
        flags = os.O_CREAT | os.O_WRONLY | os.O_APPEND
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        log_fd = os.open(log_path, flags, 0o600)
        if not stat.S_ISREG(os.fstat(log_fd).st_mode):
            os.close(log_fd)
            raise install_jobs.JobError("unsafe_log_path", "The job log is not a regular file.")
        log = os.fdopen(log_fd, "wb")
    inherited = [lock.fd for lock in DESTINATION_LOCKS if lock.fd is not None] if os.name != "nt" else []
    try:
        process = subprocess.Popen(command, cwd=KIT, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, errors="replace",
                                   start_new_session=os.name != "nt", pass_fds=inherited, env=env)
    except BaseException:
        if log:
            log.close()
        raise
    output = queue.Queue(maxsize=32)

    def read_output():
        try:
            while True:
                chunk = process.stdout.readline(65536)
                if not chunk:
                    break
                while True:
                    try:
                        output.put(chunk, timeout=0.1)
                        break
                    except queue.Full:
                        if process.poll() is not None:
                            return
        finally:
            while True:
                try:
                    output.put(None, timeout=0.1)
                    break
                except queue.Full:
                    if process.poll() is not None:
                        try:
                            output.get_nowait()
                        except queue.Empty:
                            pass

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    try:
        logged = os.fstat(log.fileno()).st_size if log else 0
        max_log_bytes = install_jobs.MAX_ATTEMPT_LOG_BYTES - install_jobs.MAX_LOG_FAILURE_RESERVE
        next_monitor = 0.0

        def monitor(force=False):
            nonlocal next_monitor
            if not build_monitor or (not force and time.monotonic() < next_monitor):
                return
            started = time.monotonic()
            current = install_jobs.tree_size(build_monitor["root"], check_cancel)
            growth = max(0, current - build_monitor["baselineBytes"])
            if growth > install_jobs.MAX_BUILD_BYTES:
                raise install_jobs.JobError("build_output_limit", "The staged build exceeded its configured output allowance.",
                    ["Review the build log and free destination space before retrying."],
                    {"path": str(build_monitor["root"]), "baselineBytes": build_monitor["baselineBytes"],
                     "currentBytes": current, "growthBytes": growth,
                     "limitBytes": install_jobs.MAX_BUILD_BYTES, "sampleIntervalSeconds": 2})
            free = shutil.disk_usage(build_monitor["volume"]).free
            if free < install_jobs.SAFETY_RESERVE_BYTES:
                raise install_jobs.JobError("disk_safety_reserve", "The destination volume reached the configured free-space reserve during the build.",
                    ["Free destination space and retry the installation."],
                    {"path": str(build_monitor["volume"]), "freeBytes": free,
                     "safetyReserveBytes": install_jobs.SAFETY_RESERVE_BYTES,
                     "sampleIntervalSeconds": 2})
            next_monitor = max(time.monotonic(), started + 2)

        with (log if log else nullcontext()) as active_log:
            while True:
                check_cancel()
                try:
                    line = output.get(timeout=0.1)
                except queue.Empty:
                    if process.poll() is not None:
                        reader.join(timeout=2)
                        if reader.is_alive():
                            if os.name != "nt":
                                try:
                                    os.killpg(process.pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                            break
                    monitor()
                    continue
                if line is None:
                    break
                if active_log and logged < max_log_bytes:
                    encoded = line.encode("utf-8", errors="replace")
                    active_log.write(encoded[:max_log_bytes - logged])
                    active_log.flush()
                    logged += min(len(encoded), max_log_bytes - logged)
                for displayed in line.splitlines() or [line]:
                    emit("log", text=displayed[:65536])
                monitor()
            monitor(force=True)
        while process.poll() is None:
            check_cancel()
            try:
                process.wait(timeout=0.1)
            except subprocess.TimeoutExpired:
                pass
            monitor()
        monitor(force=True)
        if process.returncode:
            raise BuildFailure(command, process.returncode, log_path)
    except BaseException:
        if os.name == "nt":
            if process.poll() is None:
                stopped = subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                         capture_output=True, text=True)
                if stopped.returncode and process.poll() is None:
                    emit("log", text=f"Cannot stop build process: {stopped.stdout}{stopped.stderr}")
        else:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                pass
            else:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        if os.name == "nt" or process.poll() is None:
            process.wait(timeout=10)
        raise
    finally:
        reader.join(timeout=2)
        if not reader.is_alive():
            process.stdout.close()


def installation_root(storage, game_id):
    registry_dir = install_jobs.registry_game_dir(storage, game_id)
    record = registry_dir / "registry.json"
    if record.is_file() and not record.is_symlink():
        value = install_jobs.read_versioned(record, "registry")
        if value.get("gameId") != game_id or not isinstance(value.get("destination"), str):
            raise install_jobs.JobError("invalid_registry", "The installation registry does not match this game.")
        return Path(value["destination"]).expanduser().resolve()
    return storage / game_id


def artifact(cfg, root, installed=True):
    name = cfg["game"]["app_name"]
    if platform.system() == "Darwin":
        return root / f"{name}.app/Contents/MacOS/{name}"
    directory = root / "app" if installed else root / "package" / name
    return directory / (name + (".exe" if os.name == "nt" else ""))


def original_module_relative(cfg, module):
    try:
        return Path(module["path"]).relative_to(Path(cfg["build_root"]) / "original")
    except ValueError:
        return Path(module["path"]).relative_to(cfg["developer_exe_path"].parent)


def bind_selected_auxiliary_paths(cfg, selected, root):
    paths = {}
    for module in cfg["aux_modules"]:
        role = next((item for item in selected.get("requiredRoles", [])
                     if item.get("role") == "auxiliary:" + module["key"] and item.get("found")), None)
        actual_path = role.get("actualPath") if role else None
        if (not isinstance(actual_path, str) or not actual_path or "\\" in actual_path
                or Path(actual_path).is_absolute()
                or any(part in ("", ".", "..") for part in actual_path.split("/"))):
            raise SourceError("auxiliary_path_unavailable", "A required auxiliary module has no safe installed path.",
                              ["Choose a complete game copy with its required modules."])
        paths[module["name"]] = actual_path
        module["path"] = Path(cfg["build_root"]) / "original" / Path(*actual_path.split("/"))
    selected["auxiliaryPaths"] = paths
    return paths


def sha256_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def audit_macos_bundle(bundle, executable):
    bundle = Path(bundle).resolve(strict=True)
    executable = Path(executable).resolve(strict=True)
    native_files = [executable, *sorted((bundle / "Contents" / "Frameworks").rglob("*.dylib"))]
    system_path = lambda path: path == "/usr/lib" or path.startswith("/usr/lib/") or path == "/System/Library" or path.startswith("/System/Library/")
    def resolve_reference(value, loader_dir):
        if value.startswith("@loader_path"):
            return loader_dir / value[len("@loader_path"):].lstrip("/")
        if value.startswith("@executable_path"):
            return executable.parent / value[len("@executable_path"):].lstrip("/")
        return Path(value)
    def read_rpaths(native_file):
        load_commands = subprocess.run(["otool", "-l", str(native_file)], capture_output=True,
                                       text=True, check=True).stdout.splitlines()
        rpaths = []
        for index, line in enumerate(load_commands):
            if line.strip() == "cmd LC_RPATH":
                for field in load_commands[index + 1:index + 5]:
                    if field.strip().startswith("path "):
                        rpaths.append(field.strip()[5:].split(" (offset", 1)[0])
                        break
        return rpaths
    inherited_rpaths = read_rpaths(executable)
    for native_file in native_files:
        dependencies = subprocess.run(["otool", "-L", str(native_file)], capture_output=True,
                                      text=True, check=True).stdout.splitlines()[1:]
        if native_file != executable:
            dependencies = [line for line in dependencies
                            if Path(line.strip().split(" (compatibility version", 1)[0]).name != native_file.name]
        rpaths = inherited_rpaths + read_rpaths(native_file)
        resolved_rpaths = []
        for rpath in rpaths:
            candidate = resolve_reference(rpath, native_file.parent)
            if candidate.is_absolute() and not system_path(str(candidate)):
                try:
                    canonical = candidate.resolve(strict=True)
                except OSError as error:
                    raise ValueError(f"Native runpath does not exist: {rpath} in {native_file}") from error
                if not canonical.is_relative_to(bundle):
                    raise ValueError(f"Native runpath escapes the app bundle: {rpath} in {native_file}")
            resolved_rpaths.append(candidate)
        for line in dependencies:
            dependency = line.strip().split(" (compatibility version", 1)[0]
            if dependency.startswith("/"):
                if system_path(dependency):
                    continue
                raise ValueError(f"Published app has an absolute non-system dependency: {dependency}")
            candidates = ([resolve_reference(dependency, native_file.parent)]
                          if dependency.startswith(("@loader_path", "@executable_path")) else
                          [path / dependency[len("@rpath/"):] for path in resolved_rpaths]
                          if dependency.startswith("@rpath/") else [])
            if not candidates:
                raise ValueError(f"Published app has an unsupported native dependency: {dependency}")
            found = False
            for candidate in candidates:
                if system_path(str(candidate)) and candidate.exists():
                    found = True
                    break
                try:
                    canonical = candidate.resolve(strict=True)
                except OSError:
                    continue
                if canonical.is_relative_to(bundle):
                    found = True
                    break
            if not found:
                raise ValueError(f"Published native dependency is not bundled or available in macOS: {dependency}")


def installed_manifest(cfg, game_id, job_id, executable_relative, data_root_relative, source_identity, recipe_identity,
                       app_identities, original_identity, log_path, executable_size, staged_data, auxiliary_paths):
    app_name = cfg["game"]["app_name"]
    app_path = artifact(cfg, Path())
    app_bundle = app_path.parents[2] if platform.system() == "Darwin" else Path("app")
    sidecar = sidecar_bundle(cfg)
    executable_file = Path(DATA_DIR) / executable_relative
    return {"schema": install_jobs.SCHEMA, "gameId": game_id, "jobId": job_id,
            "profileId": app_name,
            "profile": {"relativeRoot": ""},
            "paths": {"dataRoot": (Path(DATA_DIR) / data_root_relative).as_posix(),
                      "executable": (Path(DATA_DIR) / executable_relative).as_posix()},
            "launchPaths": {"game": app_path.as_posix(), "configure": f"{sidecar}/Contents/MacOS/ConfigureLauncher"},
            "fingerprints": {"executable": {"path": executable_file.as_posix(),
                                                "size": executable_size,
                                                "sha256": cfg["game"]["sha256"]},
                             "auxiliaries": [{"name": module["name"],
                                              "path": (Path(DATA_DIR) / auxiliary_paths[module["name"]]).as_posix(),
                                              "sha256": module["sha256"],
                                              "size": (staged_data / auxiliary_paths[module["name"]]).stat().st_size}
                                             for module in cfg["aux_modules"]],
                             "source": source_identity, "recipe": recipe_identity,
                             "apps": app_identities, "original": original_identity},
            "sidecars": [{"id": "configure", "role": "configure", "target": "configure",
                           "profileId": app_name}],
            "ownedArtifacts": [app_bundle.as_posix(), sidecar, DATA_DIR, ".recomp-toolkit/logs"],
            "log": log_path, "recipeIdentity": {"algorithm": recipe_identity["algorithm"],
                "sha256": recipe_identity["sha256"], "platform": recipe_identity["platform"]},
            "launch": {"executable": str(executable_relative),
                                           "sha256": cfg["game"]["sha256"],
                                           "requiredDirs": cfg.get("setup", {}).get("required_dirs", [])},
            "committedAt": int(time.time())}


def sidecar_bundle(cfg):
    return f"Configure {cfg['game']['name'].replace(':', '')}.app"


def installed_app_identities(cfg, root):
    if platform.system() == "Darwin":
        paths = [root / f"{cfg['game']['app_name']}.app",
                 root / sidecar_bundle(cfg)]
    else:
        paths = [root / "app"]
    return {path.name: install_jobs.owned_tree_identity(path) for path in paths}


def read_installation_manifest(root, missing_ok=False):
    root = Path(root).resolve(strict=True)
    metadata = root / ".recomp-toolkit"
    path = metadata / "installation.json"
    if not metadata.exists() and not metadata.is_symlink() and missing_ok:
        return {}
    if metadata.is_symlink() or not metadata.is_dir():
        raise install_jobs.JobError("unsafe_metadata_path", "The installation metadata directory is unsafe.")
    try:
        info = path.lstat()
    except FileNotFoundError:
        if missing_ok:
            return {}
        raise
    if path.is_symlink() or not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024:
        raise install_jobs.JobError("installation_manifest_invalid", "The installation manifest is not a regular file within the 1 MiB limit.")
    return install_jobs.read_versioned(path, "installation")


def validate_installed_manifest(cfg, root, manifest):
    root = Path(root).resolve(strict=True)
    metadata = root / ".recomp-toolkit"
    manifest_path = metadata / "installation.json"
    if not isinstance(manifest, dict):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed manifest root must be an object.")
    if metadata.is_symlink() or manifest_path.is_symlink() or manifest.get("gameId") != cfg["game"]["id"]:
        raise install_jobs.JobError("installation_manifest_invalid", "The installed manifest path or game identity is invalid.")
    schema = manifest.get("schema") if isinstance(manifest, dict) else None
    if (not isinstance(schema, dict) or schema.get("major") != install_jobs.SCHEMA["major"]
            or not isinstance(manifest.get("profileId"), str)
            or not manifest["profileId"] or len(manifest["profileId"]) > 64
            or any(not (char.isalnum() or char in "-_ ") for char in manifest["profileId"])):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed manifest identity is invalid.")
    profile = manifest.get("profile")
    if not isinstance(profile, dict):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed profile declaration is invalid.")
    profile_root = profile.get("relativeRoot")
    if profile_root != "" and (not isinstance(profile_root, str) or Path(profile_root).is_absolute()
                                or "\\" in profile_root or any(part in ("", ".", "..")
                                                                 for part in profile_root.split("/"))):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed profile path is unsafe.")
    launch_paths = manifest.get("launchPaths")
    sidecars = manifest.get("sidecars")
    owned = manifest.get("ownedArtifacts")
    if (not isinstance(launch_paths, dict) or not isinstance(sidecars, list) or len(sidecars) != 1
            or not isinstance(owned, list) or len(owned) != 4):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed launch declarations are invalid.")
    app_bundle = Path(f"{cfg['game']['app_name']}.app") if platform.system() == "Darwin" else Path("app")
    expected_game = artifact(cfg, root).relative_to(root)
    expected_configure = f"{sidecar_bundle(cfg)}/Contents/MacOS/ConfigureLauncher"
    for relative in [launch_paths.get("game"), launch_paths.get("configure"), *owned]:
        if (not isinstance(relative, str) or not relative or Path(relative).is_absolute() or "\\" in relative
                or any(part in ("", ".", "..") for part in relative.split("/"))):
            raise install_jobs.JobError("installation_manifest_invalid", "A launch or owned-artifact path is unsafe.")
    if (launch_paths.get("game") != expected_game.as_posix()
            or launch_paths.get("configure") != expected_configure
            or sidecars[0] != {"id": "configure", "role": "configure", "target": "configure",
                               "profileId": manifest["profileId"]}
            or set(owned) != {app_bundle.as_posix(),
                              sidecar_bundle(cfg), DATA_DIR, ".recomp-toolkit/logs"}):
        raise install_jobs.JobError("installation_manifest_invalid", "The installed launch roles differ from this build.")
    paths = manifest.get("paths", {})
    executable = paths.get("executable")
    data_root = paths.get("dataRoot")
    for relative in (executable, data_root):
        if not isinstance(relative, str) or Path(relative).is_absolute() or "\\" in relative or any(
                part in ("", ".", "..") for part in relative.split("/")):
            raise install_jobs.JobError("installation_manifest_invalid", "The installed manifest contains an unsafe relative path.")
    image = (root / executable).resolve(strict=True)
    data = (root / data_root).resolve(strict=True)
    if not image.is_relative_to(root) or not image.is_file() or not data.is_relative_to(root) or not data.is_dir():
        raise install_jobs.JobError("installation_manifest_invalid", "The installed executable or data root escapes the game folder.")
    launch = manifest.get("launch", {})
    if not isinstance(launch.get("executable"), str) or executable != (Path(DATA_DIR) / launch["executable"]).as_posix():
        raise install_jobs.JobError("installation_manifest_invalid", "The installed executable paths do not agree.")
    validate_game(root / DATA_DIR, launch["executable"], cfg["game"]["sha256"],
                  cfg.get("setup", {}).get("required_dirs", []))
    image_fact = manifest.get("fingerprints", {}).get("executable", {})
    if (image_fact.get("path") != executable or image_fact.get("size") != image.stat().st_size
            or image_fact.get("sha256") != cfg["game"]["sha256"]):
        raise install_jobs.JobError("installation_manifest_invalid", "The executable fingerprint does not match the manifest.")
    auxiliaries = manifest.get("fingerprints", {}).get("auxiliaries", [])
    if len(auxiliaries) != len(cfg["aux_modules"]):
        raise install_jobs.JobError("installation_manifest_invalid", "The auxiliary module list does not match this build.")
    for entry, module in zip(auxiliaries, cfg["aux_modules"]):
        path = entry.get("path", "")
        executable_parent = Path(executable).parent
        expected_relative = (executable_parent / module["name"]).as_posix()
        configured_relative = (executable_parent / original_module_relative(cfg, module)).as_posix()
        if (entry.get("name") != module["name"] or entry.get("sha256") != module["sha256"]
                or not isinstance(path, str) or Path(path).is_absolute() or "\\" in path
                or any(part in ("", ".", "..") for part in path.split("/"))
                or path.casefold() not in {expected_relative.casefold(), configured_relative.casefold()}):
            raise install_jobs.JobError("installation_manifest_invalid", "An auxiliary fingerprint is invalid.")
        auxiliary = (root / path).resolve(strict=True)
        if (not auxiliary.is_relative_to(root) or not auxiliary.is_file()
                or auxiliary.stat().st_size != entry.get("size")
                or sha256_file(auxiliary) != module["sha256"]):
            raise install_jobs.JobError("installation_manifest_invalid", f"An auxiliary module failed validation: {path}")
    if installed_app_identities(cfg, root) != manifest.get("fingerprints", {}).get("apps"):
        raise install_jobs.JobError("installed_app_mismatch", "The installed app does not match its manifest fingerprint.")
    return image


def manifest_identity(value):
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"
    return hashlib.sha256(encoded).hexdigest()


def remove(path):
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.exists():
        shutil.rmtree(path)


def recipe(game_dir):
    return install_jobs.recipe_identity(KIT, game_dir)


def log_event(path, stage, event, details=None, failure=False):
    try:
        install_jobs.append_job_log(path, stage, event, details, failure)
        emit("log", text=f"[{stage}] {event}" + (f" · {json.dumps(details)}" if details else ""))
    except (OSError, ValueError) as error:
        emit("log", text=f"Could not persist the {stage} job log event: {error}")


def check_space(job_paths, journal, root, phase, additional_bytes, log_path, plan):
    try:
        log_size = log_path.stat().st_size
    except FileNotFoundError:
        log_size = 0
    log_remaining = max(0, install_jobs.MAX_ATTEMPT_LOG_BYTES - log_size)
    required = max(0, additional_bytes) + install_jobs.SAFETY_RESERVE_BYTES + log_remaining
    check = install_jobs.disk_space(root, required, phase, plan)
    checks = journal.setdefault("spaceChecks", [])
    checks.append(check)
    del checks[:-32]
    journal["spaceBudget"] = plan
    install_jobs.update_job(job_paths, journal, journal.get("stage", phase),
                            spaceChecks=checks, spaceBudget=plan)
    log_event(log_path, phase, "destination space preflight passed",
              {"freeBytes": check["freeBytes"], "requiredBytes": check["requiredBytes"]})
    return check


def error_diagnostic(error, stage, root, log_path):
    log_relative = f".recomp-toolkit/logs/{Path(log_path).name}"
    if isinstance(error, InterruptedError):
        return {"code": "cancelled", "stage": "cancelled", "path": ".",
                "message": "The installation was cancelled.",
                "details": {"logPath": log_relative}, "recovery": ["resume", "choose_source"]}
    if isinstance(error, SourceError):
        result = dict(error.diagnostic)
        result["stage"] = stage
        affected = result.get("path", ".")
        if isinstance(affected, str) and Path(affected).is_absolute():
            try:
                result["path"] = Path(affected).resolve(strict=False).relative_to(Path(root).resolve()).as_posix()
            except ValueError:
                result["path"] = Path(affected).name
        result.setdefault("details", {})["logPath"] = log_relative
        return result
    if isinstance(error, BuildFailure):
        tool = error.command[1] if len(error.command) > 1 and error.command[1].endswith(".py") else error.command[0]
        return {"code": "build_failed", "stage": stage, "path": ".",
                "message": str(error), "details": {"tool": Path(tool).name,
                    "command": error.command, "returnCode": error.returncode,
                    "logPath": log_relative}, "recovery": ["open_log", "retry"]}
    if isinstance(error, install_jobs.JobError):
        details = dict(error.details)
        affected = details.get("path", ".")
        if isinstance(affected, str):
            try:
                affected = Path(affected).resolve(strict=False).relative_to(Path(root).resolve()).as_posix()
            except ValueError:
                affected = Path(affected).name if Path(affected).is_absolute() else affected
        details["logPath"] = log_relative
        return {"code": error.code, "stage": stage, "path": affected,
                "message": str(error), "details": details,
                "recovery": error.recovery or ["open_log", "retry"]}
    if isinstance(error, OSError):
        code = "insufficient_space" if error.errno == errno.ENOSPC else (
            "permission_denied" if error.errno in (errno.EACCES, errno.EPERM)
            else "filesystem_error")
        affected = os.fsdecode(error.filename) if error.filename else "."
        try:
            affected = Path(affected).resolve(strict=False).relative_to(Path(root).resolve()).as_posix()
        except (ValueError, TypeError):
            affected = Path(os.fsdecode(affected)).name if isinstance(affected, (str, bytes, os.PathLike)) else "."
        return {"code": code, "stage": stage, "path": affected, "message": str(error),
                "details": {"errno": error.errno, "logPath": log_relative},
                "recovery": ["free_space", "open_log", "retry"] if code == "insufficient_space"
                            else ["check_permissions", "open_log", "retry"]}
    return {"code": "installation_failed", "stage": stage, "path": ".",
            "message": str(error), "details": {"logPath": log_relative},
            "recovery": ["open_log", "retry"]}


def recover_publication(root, identifier):
    root = Path(root).expanduser().resolve()
    job_paths, journal = install_jobs.load_job(root, identifier)
    publication = journal.get("publication")
    manifest_path = root / ".recomp-toolkit" / "installation.json"
    if journal.get("stage") == "complete":
        committed_facts = read_installation_manifest(root, missing_ok=True)
        saved_facts = journal.get("validatedOutputs", {}).get("installed", {})
        if (committed_facts.get("jobId") != identifier or committed_facts.get("gameId") != journal.get("gameId")
                or manifest_identity(committed_facts) != manifest_identity(saved_facts)):
            raise install_jobs.JobError("completed_job_mismatch", "The completed job record does not identify its commit.")
        for key in ("staging", "backup"):
            if job_paths[key].exists():
                install_jobs.remove_owned(job_paths[key], job_paths["meta"], identifier,
                                          journal.get("ownedTokens", {}).get(key))
        return journal
    if not publication:
        return journal
    if publication.get("artifacts"):
        cfg = game_config.load(KIT / "games" / journal.get("gameId", ""), root)
        expected = ({f"{cfg['game']['app_name']}.app", sidecar_bundle(cfg), DATA_DIR}
                    if platform.system() == "Darwin" else {"app", DATA_DIR})
        if not isinstance(publication.get("artifacts"), list) or any(
                not isinstance(item, dict) or item.get("destination") not in expected
                or item.get("backup") != item.get("destination")
                for item in publication["artifacts"]):
            raise install_jobs.JobError("publication_record_invalid", "The publication journal contains an unowned artifact.")
        if len({item["destination"] for item in publication["artifacts"]}) != len(publication["artifacts"]):
            raise install_jobs.JobError("publication_record_invalid", "The publication journal repeats an artifact.")
        manifest = read_installation_manifest(root, missing_ok=True)
        if (manifest.get("jobId") == identifier and manifest.get("gameId") == journal.get("gameId")
                and manifest_identity(manifest) == publication.get("manifestIdentity")):
            if publication.get("manifest", {}) != manifest:
                raise install_jobs.JobError("publication_commit_mismatch", "The committed installation manifest differs from its journal.")
            if manifest.get("gameId") != journal.get("gameId"):
                raise install_jobs.JobError("publication_commit_mismatch", "The installed manifest belongs to another game.")
            validate_installed_manifest(cfg, root, manifest)
            for artifact in publication["artifacts"]:
                if artifact["destination"] == DATA_DIR:
                    continue
                destination = root / artifact["destination"]
                identity = install_jobs.owned_tree_identity(destination).get("sha256")
                if identity != artifact.get("stagedIdentity", {}).get("sha256"):
                    raise install_jobs.JobError("publication_artifact_mismatch", f"Committed artifact changed: {artifact['destination']}")
            for key in ("staging", "backup"):
                owned = job_paths[key]
                if owned.exists():
                    if key == "backup":
                        for artifact in publication["artifacts"]:
                            backup = owned / artifact["backup"]
                            if backup.exists():
                                if (not artifact.get("hadPrevious") or
                                        not install_jobs.matches_directory_token(backup, artifact.get("previousToken", {}))):
                                    raise install_jobs.JobError("publication_backup_mismatch", f"Backup changed: {artifact['backup']}")
                                previous = (install_jobs.payload_identity(backup).get("sha256") if artifact["destination"] == DATA_DIR
                                            else install_jobs.owned_tree_identity(backup).get("sha256"))
                                if previous != artifact.get("previousIdentity", {}).get("sha256"):
                                    raise install_jobs.JobError("publication_backup_mismatch", f"Backup identity changed: {artifact['backup']}")
                    install_jobs.remove_owned(owned, job_paths["meta"], identifier,
                                              journal.get("ownedTokens", {}).get(key))
            install_jobs.update_job(job_paths, journal, "complete", validatedOutputs={"installed": manifest}, publication=None)
            return journal
        current_manifest_identity = manifest_identity(manifest) if manifest else None
        if current_manifest_identity != publication.get("previousManifestIdentity"):
            raise install_jobs.JobError("publication_manifest_conflict", "The installed manifest changed during an interrupted publication.")
        for artifact in reversed(publication["artifacts"]):
            if not isinstance(artifact, dict) or artifact.get("destination") not in expected:
                raise install_jobs.JobError("publication_record_invalid", "The publication journal contains an unowned destination.")
            destination = root / artifact["destination"]
            backup = job_paths["backup"] / artifact.get("backup", "")
            staged = root / artifact.get("staged", "")
            for candidate, allowed in ((destination, root), (backup, job_paths["backup"]), (staged, job_paths["staging"])):
                try:
                    candidate.relative_to(allowed)
                except ValueError:
                    raise install_jobs.JobError("publication_record_invalid", "A publication artifact escaped its owned directory.")
                cursor = candidate
                while cursor != allowed:
                    if cursor.is_symlink():
                        raise install_jobs.JobError("publication_record_invalid", "A publication artifact traverses a symbolic link.")
                    cursor = cursor.parent
            if destination.exists() or destination.is_symlink():
                if install_jobs.matches_directory_token(destination, artifact.get("stagedToken", {})):
                    identity = (install_jobs.payload_identity(destination).get("sha256")
                                if artifact["destination"] == DATA_DIR
                                else install_jobs.owned_tree_identity(destination).get("sha256"))
                    if identity != artifact.get("stagedIdentity", {}).get("sha256"):
                        raise install_jobs.JobError("publication_artifact_mismatch", f"Published artifact changed during recovery: {artifact['destination']}")
                    remove(destination)
                elif artifact.get("hadPrevious") and install_jobs.matches_directory_token(destination, artifact.get("previousToken", {})):
                    pass
                else:
                    raise install_jobs.JobError("publication_conflict", f"Destination changed during recovery: {artifact['destination']}")
            if backup.exists() or backup.is_symlink():
                if not artifact.get("hadPrevious") or not install_jobs.matches_directory_token(backup, artifact.get("previousToken", {})):
                    raise install_jobs.JobError("publication_backup_mismatch", f"Backup changed during recovery: {artifact['backup']}")
                identity = (install_jobs.payload_identity(backup).get("sha256") if artifact["destination"] == DATA_DIR
                            else install_jobs.owned_tree_identity(backup).get("sha256"))
                if identity != artifact.get("previousIdentity", {}).get("sha256"):
                    raise install_jobs.JobError("publication_backup_mismatch", f"Backup identity changed: {artifact['backup']}")
                if destination.exists() or destination.is_symlink():
                    raise install_jobs.JobError("publication_conflict", f"Cannot restore backup over {artifact['destination']}.")
                backup.replace(destination)
            elif artifact.get("hadPrevious") and not destination.exists() and not destination.is_symlink():
                raise install_jobs.JobError("publication_backup_missing", f"Previous artifact is unavailable: {artifact['destination']}")
        install_jobs.update_job(job_paths, journal, "failed",
                                errors=[*journal.get("errors", []), {"code": "publication_recovered",
                                         "stage": "publishing", "message": "The previous installation was restored after an interrupted update.",
                                         "recovery": ["retry"]}], publication=None)
        return journal
    raise install_jobs.JobError("publication_record_invalid", "The publication journal has no artifact list.")

def catalog(storage, repair=False):
    games = []
    for directory in sorted((KIT / "games").iterdir()):
        if directory.name == "stub" or not (directory / "game.toml").is_file():
            continue
        item = {"id": directory.name, "title": directory.name, "state": "unavailable"}
        destination_busy = False
        recoverable_jobs = []
        try:
            game_registry_dir = install_jobs.registry_game_dir(storage, directory.name)
            registry_file = game_registry_dir / "registry.json"
            registry = {}
            if registry_file.is_file() and not registry_file.is_symlink():
                registry = install_jobs.read_versioned(registry_file, "registry")
            activity_path = game_registry_dir / "activity.json"
            activity = install_jobs.read_versioned(activity_path, "activity") if activity_path.is_file() else {}
            locator_destination = activity.get("destination", storage / directory.name)
            if activity.get("activeJobs") and isinstance(activity["activeJobs"][0], dict):
                locator_destination = activity["activeJobs"][0].get("destination", locator_destination)
            root = Path(registry.get("destination", locator_destination)).expanduser().resolve()
            if root.exists() and (root / ".recomp-toolkit").exists():
                try:
                    with install_jobs.DestinationLock(storage, lock_name="registry.lock"):
                        activity = (install_jobs.read_versioned(activity_path, "activity")
                                    if activity_path.is_file() else {})
                        remaining = []
                        for locator in activity.get("activeJobs", []):
                            identifier = locator.get("jobId") if isinstance(locator, dict) else locator
                            job_destination = locator.get("destination", root) if isinstance(locator, dict) else root
                            with install_jobs.DestinationLock(job_destination):
                                journal = recover_publication(job_destination, identifier)
                            if journal.get("stage") != "complete":
                                remaining.append(locator)
                        if remaining != activity.get("activeJobs", []):
                            activity["activeJobs"] = remaining
                            install_jobs.write_json(activity_path, activity)
                except install_jobs.JobError as error:
                    if error.code == "destination_busy":
                        destination_busy = True
                        item["detail"] = str(error)
                    else:
                        item["recoveryError"] = str(error)
            cfg = game_config.load(directory, root)
            known = bool(registry.get("destination"))
            destination = root if known else Path.home() / ("Applications" if platform.system() == "Darwin" else "Games") / directory.name
            item.update(game_entry(cfg), id=directory.name, installation=str(destination),
                        needsRust=True)
            item["state"] = "notInstalled"
            facts_path = root / ".recomp-toolkit" / "installation.json"
            facts = {}
            if not destination_busy and root.exists():
                try:
                    with install_jobs.DestinationLock(root):
                        facts = read_installation_manifest(root, missing_ok=True)
                        if facts and facts.get("gameId") != directory.name:
                            raise install_jobs.JobError("installed_identity_mismatch", "Installed manifest belongs to another game.")
                        executable_relative = facts.get("launch", {}).get("executable", cfg["game"]["executable"])
                        if (facts and artifact(cfg, root).is_file()
                                and (root / DATA_DIR / executable_relative).is_file()):
                            validate_installed_manifest(cfg, root, facts)
                            item["state"] = "ready"
                except install_jobs.JobError as error:
                    if error.code == "destination_busy":
                        destination_busy = True
                        item["detail"] = str(error)
                    else:
                        raise
            if item["state"] == "ready":
                try:
                    item["managedSource"] = str(root / DATA_DIR)
                    item["buildLog"] = str(root / facts.get("log", f".recomp-toolkit/logs/{facts.get('jobId', '')}.log"))
                    item["updateAvailable"] = facts.get("recipeIdentity", {}).get("sha256") != recipe(directory).get("sha256")
                except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
                    item["updateCheckError"] = str(error)
                if registry.get("destination") != str(root):
                    try:
                        with install_jobs.DestinationLock(storage, lock_name="registry.lock"):
                            registry_file.parent.mkdir(parents=True, exist_ok=True)
                            install_jobs.write_json(registry_file, {"schema": install_jobs.SCHEMA,
                                                   "gameId": directory.name, "destination": str(root)})
                    except (OSError, ValueError) as error:
                        item["registrationError"] = str(error)
            if activity_path.is_file() and not destination_busy:
                for locator in activity.get("activeJobs", []):
                    identifier = locator.get("jobId") if isinstance(locator, dict) else locator
                    job_destination = locator.get("destination", activity.get("destination", root)) if isinstance(locator, dict) else activity.get("destination", root)
                    try:
                        _, journal = install_jobs.load_job(job_destination, identifier)
                        if journal.get("stage") != "complete":
                            attempt = journal.get("attempt")
                            can_resume = (journal.get("stage") not in ("publishing", "cleaning", "complete")
                                          and isinstance(attempt, int) and attempt > 0)
                            actions = ["locate_missing_file", "choose_another_copy", "change_destination",
                                       "open_log"]
                            if can_resume:
                                actions.append("delete_job")
                            if can_resume:
                                actions.insert(0, "retry_failed_stage")
                            recoverable_jobs.append({"jobId": identifier, "stage": journal.get("stage"),
                                                     "destination": str(Path(job_destination).resolve()),
                                                     "source": journal.get("source"),
                                                     "candidateId": journal.get("candidateId"),
                                                     "log": journal.get("log"),
                                                     "error": journal.get("errors", [])[-1] if journal.get("errors") else None,
                                                     "canResume": can_resume, "actions": actions})
                    except (OSError, ValueError, KeyError) as error:
                        item.setdefault("recoveryErrors", []).append(str(error))
            if recoverable_jobs:
                item["recoverableJobs"] = recoverable_jobs
                if item["state"] != "ready":
                    item["state"] = "needsBuild"
            if destination_busy:
                item["state"] = "busy"
            if (item["state"] == "notInstalled" and not recoverable_jobs
                    and (cfg["code_map_path"] is None or not cfg["code_map_path"].exists())):
                item["state"] = "unavailable"
                item["detail"] = "Translation metadata is not available in this checkout."
        except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
            item["state"] = "unavailable"
            item["detail"] = str(error)
        games.append(item)
    return games


def job_log(request, storage):
    game_id = request.get("game")
    identifier = install_jobs.job_id(request.get("jobId"))
    game_dir = KIT / "games" / str(game_id)
    if game_dir.parent != KIT / "games" or game_id == "stub" or not (game_dir / "game.toml").is_file():
        raise install_jobs.JobError("unknown_game", "The requested game is unavailable.")
    registry_dir = install_jobs.registry_game_dir(storage, game_id)
    with install_jobs.DestinationLock(storage, lock_name="registry.lock"):
        activity_path = registry_dir / "activity.json"
        activity = install_jobs.read_versioned(activity_path, "activity") if activity_path.is_file() else {}
        declared = next((item for item in activity.get("activeJobs", [])
                         if isinstance(item, dict) and item.get("jobId") == identifier), None)
        if declared is not None:
            destination_input = Path(declared.get("destination", "")).expanduser()
            destination = destination_input.resolve(strict=True)
            if destination_input != destination or not destination.is_dir():
                raise install_jobs.JobError("unsafe_job_destination", "The saved job destination is not canonical.")
        else:
            registry_path = registry_dir / "registry.json"
            if not registry_path.is_file():
                raise install_jobs.JobError("unknown_job", "The requested job is not registered for this game.")
            registry = install_jobs.read_versioned(registry_path, "registry")
            if registry.get("gameId") != game_id or not isinstance(registry.get("destination"), str):
                raise install_jobs.JobError("invalid_registry", "The installation registry does not match this game.")
            destination_input = Path(registry["destination"]).expanduser()
            destination = destination_input.resolve(strict=True)
            if destination_input != destination or not destination.is_dir():
                raise install_jobs.JobError("unsafe_job_destination", "The saved job destination is not canonical.")
            facts_path = destination / ".recomp-toolkit" / "installation.json"
            if not facts_path.is_file() or read_installation_manifest(destination).get("jobId") != identifier:
                raise install_jobs.JobError("unknown_job", "The requested job is not registered for this game.")
        with install_jobs.DestinationLock(destination):
            job_paths, journal = install_jobs.load_job(destination, identifier)
            attempt = journal.get("attempt")
            expected_name = f"{identifier}-{attempt}.log"
            expected_log = f".recomp-toolkit/logs/{expected_name}"
            if (type(attempt) is not int or attempt < 1 or journal.get("log") != expected_log
                    or journal.get("gameId") != game_id
                    or Path(journal.get("destination", "")).resolve() != destination):
                raise install_jobs.JobError("invalid_job_log", "The saved job log reference is invalid.")
            log_path = job_paths["logs"] / expected_name
            if (job_paths["logs"].is_symlink() or log_path.is_symlink()
                    or log_path.resolve(strict=True).parent != job_paths["logs"].resolve()
                    or not stat.S_ISREG(log_path.stat(follow_symlinks=False).st_mode)):
                raise install_jobs.JobError("unsafe_job_log", "The saved job log is not a safe regular file.")
            flags = os.O_RDONLY
            if hasattr(os, "O_NOFOLLOW"):
                flags |= os.O_NOFOLLOW
            fd = os.open(log_path, flags)
            try:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode):
                    raise install_jobs.JobError("unsafe_job_log", "The saved job log is not a safe regular file.")
                size = info.st_size
                os.lseek(fd, max(0, size - 256 * 1024), os.SEEK_SET)
                content = os.read(fd, 256 * 1024 + 1)
            finally:
                os.close(fd)
            truncated = size > 256 * 1024
            text = content[-256 * 1024:].decode("utf-8", errors="replace")
            return {"jobId": identifier, "text": text, "truncated": truncated,
                    "log": expected_log}


def prerequisites(needs_rust, needs_innoextract=False, needs_unshield=False):
    checks = []
    bundled_dependencies = (KIT.parent / "dependencies/manifest.json").is_file()
    brew = shutil.which("brew")
    if not brew and os.name != "nt":
        brew = next((str(path) for path in (Path("/opt/homebrew/bin/brew"), Path("/usr/local/bin/brew"),
                                           Path("/home/linuxbrew/.linuxbrew/bin/brew")) if path.is_file()), None)
    if brew:
        try:
            prefix = subprocess.run([brew, "--prefix"], capture_output=True, text=True, timeout=15)
            if prefix.returncode or not prefix.stdout.strip():
                brew = None
            else:
                base = Path(prefix.stdout.strip())
                paths = [base / "bin", base / "opt/llvm/bin", base / "opt/make/libexec/gnubin"]
                os.environ["PATH"] = os.pathsep.join([os.environ.get("PATH", ""), *map(str, paths)])
        except (OSError, subprocess.TimeoutExpired):
            brew = None
    tools = [("clang", "C compiler", "https://releases.llvm.org/"),
             ("clang++", "C++ compiler", "https://releases.llvm.org/"),
             ("cmake", "CMake", "https://cmake.org/download/"),
             ("ninja", "Ninja", "https://ninja-build.org/"),
             ("git", "Git", "https://git-scm.com/downloads")]
    if not bundled_dependencies:
        tools += [("cargo", "Cargo", "https://rustup.rs/"),
                  ("rustc", "Rust compiler", "https://rustup.rs/")]
    if bundled_dependencies:
        translator = KIT.parent / "dependencies/bin/recomp-translate"
        checks.append({"label": "Game translator", "ok": translator.is_file(),
                       "detail": "Installed" if translator.is_file() else "Bundled translator is missing",
                       "help": "Reinstall Recomp Toolkit to restore its bundled translator.",
                       "command": "", "url": ""})
    if needs_unshield:
        try:
            unshield_library()
            checks.append({"label": "InstallShield extraction", "ok": True, "detail": "Installed",
                           "help": "", "command": "", "url": ""})
        except InnoError as error:
            checks.append({"label": "InstallShield extraction", "ok": False, "detail": str(error),
                           "help": "Reinstall Recomp Toolkit or install libunshield, then check again.",
                           "command": "", "url": "https://github.com/twogood/unshield"})
    if needs_innoextract:
        tools += [("innoextract", "GOG installer extraction", "https://constexpr.org/innoextract/")]
    if os.name != "nt" and not bundled_dependencies:
        tools += [("make", "Make (video dependency)", "https://www.gnu.org/software/make/")]
    def shell_command(arguments):
        return subprocess.list2cmdline(arguments) if os.name == "nt" else shlex.join(arguments)

    for command, label, url in tools:
        check_cancel()
        local = Path(sys.executable).parent / (command + (".exe" if os.name == "nt" else ""))
        found = str(local) if local.is_file() else shutil.which(command)
        ok, detail = False, "Not found"
        if found:
            try:
                version = subprocess.run([found, "--version"], capture_output=True, text=True, timeout=15)
                ok = version.returncode == 0
                detail = (version.stdout or version.stderr).splitlines()[0] if (version.stdout or version.stderr) else found
            except (OSError, subprocess.TimeoutExpired) as error:
                detail = str(error)
        help = f"Open the setup guide, install {label}, then restart the library and choose Check again."
        install_command = ""
        if command in ("cmake", "ninja"):
            help = f"{label} helps build your game app. Open {'Command Prompt' if os.name == 'nt' else 'Terminal'}, paste the command below and press Enter. When it finishes, choose Check again."
            install_command = shell_command([sys.executable, "-m", "pip", "install", command])
        elif platform.system() == "Darwin" and command in ("clang", "clang++", "git", "make"):
            help = "Open Terminal (Applications → Utilities), paste the command below and press Enter. Choose Install in the window that appears. When it finishes, choose Check again."
            install_command = "xcode-select --install"
            url = "https://developer.apple.com/documentation/xcode/installing-the-command-line-tools/"
        elif command in ("cargo", "rustc"):
            help = "Open the Rust setup guide and follow the instructions for your computer. Rust builds the game translator and graphics support. Restart the library when setup finishes, then choose Check again."
        elif command == "innoextract":
            help = "Install innoextract using the setup guide to unpack your GOG installer. Restart the library afterwards. You can also go back and choose an installed game folder or ZIP, which needs no extra extraction tool."
        elif command in ("clang", "clang++"):
            help = "Install LLVM using the setup guide. On Windows, enable its option to add LLVM to PATH. PATH lets the library find installed tools. Restart the library, then choose Check again."
        methods = []
        package = {"clang": "llvm", "clang++": "llvm", "cargo": "rust", "rustc": "rust"}.get(command, command)
        if brew:
            methods.append({"label": "Homebrew", "help": "Open Terminal, paste this command and press Enter. When it finishes, choose Check again.",
                            "command": shell_command([brew, "install", package]),
                            "url": f"https://formulae.brew.sh/formula/{package}"})
        if url:
            methods.append({"label": "Download", "help": f"Open the official {label} page and follow the instructions for your system. Restart the library after setup, then choose Check again.", "url": url})
        methods.append({"label": "Manual", "help": help, "command": install_command, "url": url})
        checks.append({"label": label, "ok": ok, "detail": detail, "methods": methods})
    if platform.system() == "Darwin":
        sdk = subprocess.run(["/usr/bin/xcrun", "--show-sdk-path"], capture_output=True, text=True, timeout=15)
        checks.append({"label": "Apple command line tools", "ok": sdk.returncode == 0,
                       "detail": sdk.stdout.strip() if sdk.returncode == 0 else "Install with xcode-select --install",
                       "url": "https://developer.apple.com/documentation/xcode/installing-the-command-line-tools/",
                       "help": "Open Terminal (Applications → Utilities), paste this command and press Enter. Choose Install in the window that appears, then check again when it finishes.",
                       "command": "xcode-select --install"})
    modules = ("pefile", "PIL", "resvg_py")
    missing = [module for module in modules if importlib.util.find_spec(module) is None]
    checks.append({"label": "Game conversion tools", "ok": not missing,
                   "detail": ", ".join(missing) if missing else "Installed", "url": "",
                   "help": f"Open {'Command Prompt' if os.name == 'nt' else 'Terminal'}, paste the command below and press Enter. This installs the tools that read and convert your game. When it finishes, choose Check again.",
                   "command": shell_command([sys.executable, "-m", "pip", "install", "-r", str(KIT / "tools/requirements-library.txt")])})
    for check in checks:
        if "methods" not in check:
            check["methods"] = [{"label": "Manual", "help": check["help"], "command": check["command"]}]
            if check["url"]:
                check["methods"].append({"label": "Download", "help": "Open the official setup page and follow the instructions for your system.", "url": check["url"]})
    return {"checks": checks, "ready": all(check["ok"] for check in checks), "homebrew": brew,
            "python": sys.executable, "platform": platform.system()}


def archive_adapter(cfg, source_kind):
    if source_kind == "inno":
        return InnoAdapter(cfg["setup"]["installers"])
    if source_kind == "installshield":
        return UnshieldAdapter(cfg)
    return zip_adapter


def media_guidance(source, kind):
    if source.suffix.casefold() == ".bin":
        stem = re.sub(r"-\d+[a-z]?$", "", source.stem, flags=re.IGNORECASE)
        with os.scandir(source.parent) as siblings:
            for index, entry in enumerate(siblings):
                check_cancel()
                if index >= 4096:
                    break
                if (entry.name.casefold() == (stem + ".exe").casefold() and not entry.is_symlink()
                        and entry.is_file(follow_symlinks=False)
                        and probe_bytes(entry.path, check_cancel)["kind"] in ("inno", "unsupported_inno")):
                    return _source_diagnostic(source, "installer_payload_selected",
                        "This BIN appears to be a GOG installer payload. Choose its matching setup EXE.",
                        [f"Choose {entry.name} and keep its BIN files beside it."],
                        {"setupExecutable": entry.name}, "installer_payload")
    if kind in ("iso9660", "raw_cd", "cue", "media_hint"):
        message = {
            "iso9660": "This is an ISO disc image. Extract or mount it before choosing the game files.",
            "raw_cd": "This is raw CD sector data. Use its matching CUE file to extract the data track.",
            "cue": "This CUE describes a disc image. Keep its BIN files beside it when extracting.",
            "media_hint": "The filename suggests a disc image or installer payload, but its format could not be verified.",
        }[kind]
        recovery = ["Extract ISO images with 7-Zip. On macOS, you can also try opening the ISO in Finder."]
        tools = [{"label": "7-Zip", "url": "https://www.7-zip.org/download.html"}]
        if kind in ("raw_cd", "cue", "media_hint"):
            recovery.append("For BIN/CUE disc images, convert the data track to ISO with bchunk, then extract the ISO. A BIN extension alone does not identify a disc image.")
            tools.append({"label": "bchunk", "url": "https://github.com/hessu/bchunk"})
        recovery.append("Then choose the extracted game folder, or Setup.exe/data1.cab if the disc contains an InstallShield installer.")
        result = _source_diagnostic(source, "unsupported_media", message, recovery,
                                    {"suggestedTools": tools, "formatVerified": kind != "media_hint"}, kind)
        return result
    return None


def inspect_source(cfg, source):
    source = Path(source).expanduser()
    try:
        source = source.resolve(strict=True)
    except OSError as error:
        raise SourceError("source_unavailable", "The selected source is missing or unreadable.",
                          ["Check the source path and try again."], str(source), {"error": str(error)}) from error
    if source.is_dir():
        probe = {"kind": "folder"}
    elif source.is_file():
        probe = probe_bytes(source, check_cancel)
    else:
        raise SourceError("unsupported_source", "Choose a game folder, supported archive, or game installer.",
                          ["Choose an installed game folder or a supported source file."], str(source))
    if probe["kind"] in ("folder", "pe"):
        executable = source if probe["kind"] == "pe" else None
        result = inspect_folder(cfg, source, check_cancel, selected_executable=executable,
                                include_installers=True)
        installers = result["source"].pop("installers", [])
        if len(installers) == 1:
            return inspect_source(cfg, Path(installers[0]))
        if len(installers) > 1:
            return _source_diagnostic(source, "installer_selection_required",
                "More than one installer was found. Choose the installer to inspect.",
                ["Choose an installer below, or select its setup file directly."],
                {"installers": installers}, "folder")
        if executable is not None:
            result["source"]["probe"] = probe["evidence"]
            return _add_sibling_facts(result, source)
        return result
    if probe["kind"] == "installshield_cab":
        probe["kind"] = "installshield"
    if probe["kind"] in ("zip", "inno", "installshield"):
        kind = probe["kind"]
        adapter = archive_adapter(cfg, kind)
        try:
            digest = adapter.archive_digest(source, check_cancel)
            with tempfile.TemporaryDirectory(prefix="recomp-toolkit-inspect-") as temporary:
                extracted = Path(temporary) / "content"
                if kind in ("inno", "installshield"):
                    result = adapter.inspect(cfg, source, extracted, check_cancel)
                else:
                    adapter.extract(source, extracted, check_cancel)
                    result = inspect_folder(cfg, extracted, check_cancel)
                if adapter.archive_digest(source, check_cancel) != digest:
                    raise SourceError("source_changed", "The archive changed while it was being inspected.",
                                      ["Inspect the archive again and retry the import."], str(source))
            _set_archive_candidate_ids(result, digest)
            result["source"].update(kind=kind, path=str(source), archiveSha256=digest)
            if kind == "zip":
                result["limits"].update({"maxArchiveEntries": 20000,
                                         "maxArchiveBytes": 4 * 1024 * 1024 * 1024})
            return result
        except (ZipError, InnoError) as error:
            return _source_diagnostic(source, error.code, str(error),
                                      ["Check the selected source and choose the file again, or use an installed game folder."],
                                      source_kind=kind)
        except InterruptedError:
            raise
        except (OSError, zipfile.BadZipFile, RuntimeError) as error:
            return _source_diagnostic(source, "unreadable_archive", "The archive could not be read safely.",
                                      ["Check the archive and try again."], {"error": str(error)}, kind)
    guidance = media_guidance(source, probe["kind"])
    if guidance:
        return _add_sibling_facts(guidance, source)
    messages = {
        "zip": ("unsupported_archive", "The ZIP did not contain a supported game copy.",
                ["Choose a complete installed game folder or a ZIP containing the game files."]),
        "unsupported_inno": ("unsupported_installer_version", "An Inno Setup installer was detected with an unsupported loader signature.",
                             ["Choose an installed game folder or a supported installer version."]),
        "probe_limit": ("source_probe_limit", "The source signature could not be verified within the inspection limit.",
                        ["Choose an installed game folder or a smaller source file."]),
        "cabinet": ("unsupported_media", "A Cabinet archive was recognized, but no verified import adapter is available.",
                    ["Choose an installed game folder."]),
        "unreadable": ("unreadable_source", "The selected source could not be read.",
                       ["Check the source file and try again."]),
        "unknown": ("unsupported_source", "The selected file is not a recognized game executable or supported archive.",
                    ["Choose an installed game folder or a supported source file."]),
    }
    code, message, recovery = messages.get(probe["kind"], messages["unknown"])
    result = _source_diagnostic(source, code, message, recovery, {"probe": probe["evidence"],
                                                                  "sourceKind": probe["kind"]}, probe["kind"])
    return _add_sibling_facts(result, source)


def _set_archive_candidate_ids(result, digest, clear_content_root=True):
    replacements = {}
    for candidate in result["candidates"]:
        member_root = candidate["contentRootRelative"]
        previous_id = candidate["id"]
        candidate["id"] = hashlib.sha256((digest + "\0" + member_root + "\0"
                                           + candidate["executable"]["path"] + "\0"
                                           + str(candidate["executable"].get("sha256"))).encode()).hexdigest()[:24]
        replacements[previous_id] = candidate["id"]
        candidate["archiveMemberRoot"] = member_root
        candidate["archiveSha256"] = digest
        if clear_content_root:
            candidate["contentRoot"] = ""
    for diagnostic in result["diagnostics"]:
        previous_id = diagnostic.get("candidateId")
        if previous_id in replacements:
            diagnostic["candidateId"] = replacements[previous_id]


def _add_sibling_facts(result, source):
    siblings, diagnostics = sibling_payload_facts(source, check_cancel)
    result["source"]["siblings"] = siblings
    try:
        info = Path(source).stat()
        result["source"]["selectedFile"] = {"name": Path(source).name,
                                            "extensionHint": Path(source).suffix.casefold().lstrip("."),
                                            "size": info.st_size, "metadataOnly": True}
    except OSError as error:
        diagnostics.append({"code": "source_changed", "stage": "inspecting", "path": str(source),
                            "message": "The selected source changed while its metadata was inspected.",
                            "details": {"error": str(error)},
                            "recovery": ["Check the source file and inspect it again."]})
    result["diagnostics"].extend(diagnostics)
    if diagnostics:
        result["scanComplete"] = False
    result["limits"].update({"maxProbeBytes": 4 * 1024 * 1024,
                             "maxSiblingEntries": 4096, "maxSiblings": 64})
    return result


def _source_diagnostic(source, code, message, recovery, details=None, source_kind="file"):
    return {"schema": {"major": 1, "minor": 0},
            "source": {"kind": source_kind, "path": str(source),
                       "identity": {"payload": "pending", "reason": "The source is not yet prepared."}},
            "candidates": [], "diagnostics": [{"code": code, "stage": "inspecting", "path": str(source),
                "message": message, "details": details or {}, "recovery": recovery}],
            "limits": {"maxProbeBytes": 4 * 1024 * 1024}, "scanComplete": True}


def select_candidate(result, candidate_id=None, explicit_source=None):
    candidates = result.get("candidates", [])
    if explicit_source is not None:
        wanted = Path(explicit_source).resolve()
        explicit_candidate = None
        for item in candidates:
            image = Path(item["contentRoot"]) / Path(*item["executable"]["path"].split("/"))
            if image.resolve() == wanted:
                explicit_candidate = item
                break
        if candidate_id and (not explicit_candidate or explicit_candidate["id"] != candidate_id):
            raise SourceError("invalid_candidate", "The selected ID does not refer to the explicitly selected executable.",
                              ["Inspect the selected executable again and choose its game copy."], str(wanted))
        if explicit_candidate and explicit_candidate.get("installable"):
            return explicit_candidate
        raise SourceError("selected_executable_unavailable", "The selected executable is not a complete supported game copy.",
                          ["Choose the installed game executable with its full content folder."], str(wanted),
                          {"diagnostics": explicit_candidate.get("diagnostics", []) if explicit_candidate else []})
    if candidate_id is not None:
        chosen = next((item for item in candidates if item["id"] == candidate_id), None)
        if not chosen or not chosen.get("installable"):
            raise SourceError("invalid_candidate", "The selected game copy is no longer available or complete.",
                              ["Inspect the source again and choose a complete supported game copy."],
                              details={"candidateId": candidate_id})
        return chosen
    if not result.get("scanComplete", False):
        raise SourceError("inspection_incomplete", "The source contains too many entries to select a game copy safely.",
                          ["Choose the installed game folder or select its game executable directly."],
                          details=result.get("limits", {}))
    viable = [item for item in candidates if item.get("installable")]
    if len(viable) == 1:
        return viable[0]
    if len(viable) > 1:
        raise SourceError("candidate_selection_required", "More than one supported game copy was found.",
                          ["Inspect the source and choose which game copy to import."],
                          details={"candidates": [{"id": item["id"], "path": item["contentRootRelative"],
                                                   "executable": item["executable"]["path"]}
                                                  for item in viable]})
    if not candidates and result.get("diagnostics"):
        diagnostic = result["diagnostics"][0]
        raise SourceError(diagnostic["code"], diagnostic["message"], diagnostic["recovery"],
                          diagnostic.get("path"), diagnostic.get("details"))
    raise SourceError("no_supported_candidate", "No complete supported game copy was found in this source.",
                      ["Choose a complete installed game folder or a supported installer for this game."],
                      details={"candidates": [{"id": item["id"], "status": item["status"],
                                               "diagnostics": item["diagnostics"]} for item in candidates]})


def prepare_install(request, storage):
    validate_inspection_request(request)
    game_id = request["game"]
    directory = KIT / "games" / game_id
    if directory.parent != KIT / "games" or not (directory / "game.toml").is_file() or game_id == "stub":
        raise ValueError("Unknown game")
    source_path = Path(request["source"]).expanduser()
    try:
        source_path = source_path.resolve(strict=True)
    except OSError as error:
        raise SourceError("source_unavailable", "The selected source is missing or unreadable.",
                          ["Check the source path and try again."], str(source_path), {"error": str(error)}) from error
    previous_root = installation_root(storage, game_id).resolve()
    root = Path(request.get("destination") or previous_root).expanduser().resolve()
    cfg = game_config.load(directory, root)
    inspection = inspect_source(cfg, source_path)
    source_path = Path(inspection["source"]["path"])
    source_kind = inspection["source"]["kind"]
    explicit_source = source_path if source_kind == "executable" else None
    selected = select_candidate(inspection, request.get("candidateId"), explicit_source)
    executable_relative = Path(*selected["executable"]["path"].split("/"))
    bind_selected_auxiliary_paths(cfg, selected, root)
    if cfg["code_map_path"] is None or not cfg["code_map_path"].exists():
        raise ValueError("This game has no translation metadata")
    if cfg["game"].get("developer_exe"):
        raise ValueError("Library installation requires a recipe without a fixed developer_exe")
    source_root = Path(selected["contentRoot"]) if source_kind in ("folder", "executable") else None
    data = root / DATA_DIR
    if source_root is not None:
        if Path(storage).expanduser().resolve().is_relative_to(source_root.resolve()):
            raise SourceError("storage_source_overlap",
                              "The library storage folder is inside the selected game source.",
                              ["Choose storage outside the source folder before importing."],
                              str(storage), {"source": str(source_root)})
        destination_path = root.resolve()
        source_path_resolved = source_root.resolve()
        inside_destination = source_path_resolved.is_relative_to(destination_path)
        destination_inside_source = destination_path.is_relative_to(source_path_resolved)
        exact_managed_data = source_path_resolved == data.resolve() and (root / ".recomp-toolkit" / "installation.json").is_file()
        if ((inside_destination or destination_inside_source) and not exact_managed_data):
            raise SourceError("source_destination_overlap",
                              "The selected source overlaps the library destination or its cleanup folders.",
                              ["Choose a complete game folder outside the library destination."],
                              str(source_root), {"destination": str(root)})
    elif source_kind in ("zip", "inno", "installshield"):
        if source_path.is_relative_to(root):
            raise SourceError("source_destination_overlap",
                              "The selected archive overlaps the library destination.",
                              ["Choose an archive and destination in separate folders."], str(source_path),
                              {"destination": str(root)})
    facts = root / ".recomp-toolkit" / "installation.json"
    unrelated = []
    if root.exists() and not facts.is_file():
        for entry in root.iterdir():
            if entry.name != ".recomp-toolkit":
                unrelated.append(entry.name)
                continue
            if entry.is_symlink() or not entry.is_dir():
                unrelated.append(entry.name)
                continue
            if any(child.name not in {"install.lock", "jobs", "logs", "staging-" + str(request.get("jobId", "")),
                                      "backup-" + str(request.get("jobId", ""))}
                   for child in entry.iterdir()):
                unrelated.append(entry.name)
    if unrelated:
        raise SourceError("legacy_install_requires_recreate",
                          "This folder contains a legacy installation that the new job system cannot safely update.",
                          ["Choose an empty destination. Existing files will not be changed."], str(root),
                          {"entries": unrelated})
    if facts.is_file():
        saved = read_installation_manifest(root)
        if saved.get("gameId") != game_id:
            raise SourceError("destination_game_mismatch", "This destination belongs to a different game.",
                              ["Choose the original destination or an empty folder."], str(root))
        if source_kind not in ("folder", "executable") or Path(selected["contentRoot"]).resolve() != data.resolve():
            raise SourceError("managed_install_rebuild_source_required",
                              "An existing installation can only be rebuilt from its managed original copy.",
                              ["Choose the installed original folder or choose an empty destination."],
                              str(selected["contentRoot"]), {"managedSource": str(data), "destination": str(root)})
    return {"gameId": game_id, "directory": directory, "sourcePath": source_path,
            "root": root, "cfg": cfg,
            "sourceKind": source_kind, "explicitSource": explicit_source, "selected": selected}


def install(request, storage, prepared=None):
    prepared = prepared or prepare_install(request, storage)
    game_id = prepared["gameId"]
    directory = prepared["directory"]
    source_path = prepared["sourcePath"]
    root = prepared["root"]
    cfg = prepared["cfg"]
    source_kind = prepared["sourceKind"]
    adapter = archive_adapter(cfg, source_kind)
    explicit_source = prepared["explicitSource"]
    selected = prepared["selected"]
    bind_selected_auxiliary_paths(cfg, selected, root)
    executable_relative = Path(*selected["executable"]["path"].split("/"))
    exclude_paths = prepared.get("excludePaths", [])
    data = root / DATA_DIR
    emit("stage", stage="checking", message="Checking build prerequisites")
    checks = prerequisites(True, source_kind == "inno", source_kind == "installshield")
    if not checks["ready"]:
        raise ValueError("Missing prerequisites: " + ", ".join(c["label"] for c in checks["checks"] if not c["ok"]))
    check_cancel()
    install_jobs.ensure_metadata(root)
    job_identifier = request.get("jobId")
    recipe_facts = recipe(directory)
    emit("stage", stage="fingerprinting", message="Reading your game files")
    source_identity = ({"kind": source_kind, "sha256": adapter.archive_digest(source_path, check_cancel)}
                       if source_kind in ("zip", "inno", "installshield") else
                       {"kind": "folder", **install_jobs.payload_identity(selected["contentRoot"], check_cancel,
                                                                                exclude_paths=exclude_paths,
                                                                                progress=file_progress())})
    archive_bytes = adapter.archive_uncompressed_size(source_path, check_cancel) if source_kind in ("zip", "inno", "installshield") else 0
    installer_bytes = adapter.source_size(source_path) if source_kind in ("inno", "installshield") else 0
    copy_estimate = source_identity.get("bytes", archive_bytes)
    if job_identifier:
        job_paths, journal = install_jobs.load_job(root, job_identifier)
        if (journal.get("gameId") != game_id or journal.get("candidateId") != selected["id"]
                or journal.get("sourceIdentity") != source_identity
                or journal.get("recipeIdentity", {}).get("sha256") != recipe_facts.get("sha256")):
            raise SourceError("resume_identity_changed", "The source or build recipe changed since this job was saved.",
                              ["Choose the original source and retry, or start a new installation."],
                              details={"jobId": job_identifier, "recovery": ["restart", "choose_source"]})
        if journal.get("publication"):
            journal = recover_publication(root, job_identifier)
            job_paths, journal = install_jobs.load_job(root, job_identifier)
        if journal.get("stage") == "complete":
            return {"game": game_id, "state": "ready", "jobId": job_identifier}
        if journal.get("stage") in ("publishing", "cleaning"):
            raise SourceError("job_requires_recovery", "This job is at a publication recovery point.",
                              ["Refresh the library to recover the saved installation state."],
                              details={"jobId": job_identifier, "stage": journal.get("stage")})
        if job_paths["backup"].exists() and not journal.get("publication"):
            install_jobs.remove_owned(job_paths["backup"], job_paths["meta"], job_identifier,
                                      journal.get("ownedTokens", {}).get("backup"))
        journal["attempt"] = int(journal.get("attempt", 1)) + 1
    else:
        job_paths, journal = install_jobs.create_job(root, game_id,
            {"kind": source_kind, "path": str(source_path)}, source_identity, recipe_facts,
            selected["id"])
        activity_dir = install_jobs.registry_game_dir(storage, game_id)
        activity_dir.mkdir(parents=True, exist_ok=True)
        activity_path = activity_dir / "activity.json"
        activity = install_jobs.read_versioned(activity_path, "activity") if activity_path.is_file() else {
            "schema": install_jobs.SCHEMA, "activeJobs": []}
        active_jobs = []
        for locator in activity.get("activeJobs", []):
            identifier = locator.get("jobId") if isinstance(locator, dict) else locator
            destination = locator.get("destination", activity.get("destination", root)) if isinstance(locator, dict) else activity.get("destination", root)
            try:
                _, existing = install_jobs.load_job(destination, identifier)
                if existing.get("stage") != "complete":
                    active_jobs.append({"jobId": identifier, "destination": str(Path(destination).resolve())})
            except (OSError, ValueError):
                active_jobs.append({"jobId": identifier, "destination": str(Path(destination).resolve())})
        active_jobs.append({"jobId": journal["jobId"], "destination": str(root)})
        activity["activeJobs"] = active_jobs
        activity.pop("destination", None)
        try:
            install_jobs.write_json(activity_path, activity)
        except BaseException:
            install_jobs.remove_owned(job_paths["staging"], job_paths["meta"], journal["jobId"],
                                      journal.get("ownedTokens", {}).get("staging"))
            if not job_paths["journal"].is_symlink():
                job_paths["journal"].unlink(missing_ok=True)
                install_jobs._sync_directory(job_paths["jobs"])
            raise
    job_identifier = journal["jobId"]
    log_dir = job_paths["logs"]
    log_path = log_dir / f"{job_identifier}-{journal.get('attempt', 1)}.log"
    journal["log"] = f".recomp-toolkit/logs/{log_path.name}"
    space_plan = {"policy": "install-space-v1", "volume": str(root),
                  "archiveExpansionEstimateBytes": archive_bytes,
                  "installerSnapshotBytes": installer_bytes,
                  "sourceCopyEstimateBytes": copy_estimate,
                  "buildAllowanceBytes": install_jobs.MAX_BUILD_BYTES,
                  "safetyReserveBytes": install_jobs.SAFETY_RESERVE_BYTES,
                  "attemptLogMaxBytes": install_jobs.MAX_ATTEMPT_LOG_BYTES,
                  "retainedLogMaxBytes": install_jobs.MAX_RETAINED_LOG_BYTES,
                  "retainedLogsAdditionalBytes": 0,
                  "retainedBackupAdditionalBytes": 0,
                  "existingAllocatedBytesExcludedByFilesystemFreeSpace": True,
                  "buildMonitorSampleIntervalSeconds": 2,
                  "monitorRoots": [".recomp-toolkit/" + job_paths["staging"].name,
                                   ".recomp-toolkit/" + job_paths["logs"].name],
                  "externalCachesOutsideMonitor": ["user toolchain/configuration caches"]}
    install_jobs.update_job(job_paths, journal, journal.get("stage", "inspecting"), log=journal["log"])
    stage = "copying"
    committed = False
    emit("stage", stage="copying", message="Installation job started",
         jobId=job_identifier, destination=str(root))
    try:
        install_jobs.rotate_logs(log_dir, current=log_path)
        log_event(log_path, "inspecting", "installation attempt started",
                  {"jobId": job_identifier, "attempt": journal.get("attempt", 1)})
        staging = job_paths["staging"]
        build_root = staging / "build"
        staged_data = build_root / "original"
        install_jobs.update_job(job_paths, journal, "failed")
        if staging.exists():
            install_jobs.remove_owned(staging, job_paths["meta"], job_identifier,
                                      journal.get("ownedTokens", {}).get("staging"))
        journal.setdefault("ownedTokens", {})["staging"] = None
        install_jobs.update_job(job_paths, journal, journal.get("stage", "copying"),
                                ownedTokens=journal["ownedTokens"])
        staging.mkdir()
        info = staging.stat()
        install_jobs.write_json(staging / "owner.json", {"schema": install_jobs.SCHEMA,
                                 "jobId": job_identifier, "device": info.st_dev, "inode": info.st_ino})
        journal.setdefault("ownedTokens", {})["staging"] = install_jobs.directory_token(staging)
        install_jobs.update_job(job_paths, journal, journal.get("stage", "copying"),
                                ownedTokens=journal["ownedTokens"])
        build_root.mkdir()
        if source_kind in ("zip", "inno", "installshield"):
            stage = "extracting"
            emit("stage", stage=stage, message="Extracting the game archive")
            install_jobs.update_job(job_paths, journal, stage)
            check_space(job_paths, journal, root, "extracting",
                        archive_bytes + installer_bytes + copy_estimate + install_jobs.MAX_BUILD_BYTES,
                        log_path, space_plan)
            log_event(log_path, stage, "archive extraction started",
                      {"declaredExpansionBytes": archive_bytes})
            extracted = staging / "extracted"
            try:
                adapter.extract(source_path, extracted, check_cancel,
                                **({"progress": file_progress(archive_bytes)} if source_kind in ("inno", "installshield") else {}))
            except (ZipError, InnoError) as error:
                raise SourceError(error.code, str(error), ["Check the archive and inspect it again."], str(source_path)) from error
            extracted_result = inspect_folder(cfg, extracted, check_cancel)
            _set_archive_candidate_ids(extracted_result, source_identity["sha256"], clear_content_root=False)
            selected_now = select_candidate(extracted_result, selected["id"])
            source = Path(selected_now["contentRoot"])
            if adapter.archive_digest(source_path, check_cancel) != selected.get("archiveSha256"):
                raise SourceError("source_changed", "The archive changed after its contents were inspected.",
                                  ["Inspect the archive again and retry the import."], str(source_path))
            if adapter.archive_digest(source_path, check_cancel) != source_identity["sha256"]:
                raise SourceError("source_changed", "The archive changed while its contents were being extracted.",
                                  ["Inspect the archive again and retry the import."], str(source_path))
        elif source_kind not in ("zip", "inno", "installshield"):
            source = Path(selected["contentRoot"])
            current = inspect_source(cfg, source_path)
            selected_now = select_candidate(current, selected["id"], explicit_source)
            source = Path(selected_now["contentRoot"])
            if selected_now["executable"].get("sha256") != selected["executable"].get("sha256"):
                raise SourceError("source_changed", "The selected executable changed after source inspection.",
                                  ["Inspect the source again and retry the import."], str(source_path))
        emit("stage", stage="validating", message="Verifying the supported game version")
        stage = "validating"
        install_jobs.update_job(job_paths, journal, stage)
        required = cfg.get("setup", {}).get("required_dirs", [])
        validate_game(source, executable_relative, cfg["game"]["sha256"], required)
        for module in cfg["aux_modules"]:
            image = source / selected["auxiliaryPaths"][module["name"]]
            if not image.is_file() or hashlib.sha256(image.read_bytes()).hexdigest() != module["sha256"]:
                raise ValueError(f"Missing or unsupported auxiliary module: {module['name']}")
        check_cancel()
        stage = "copying"
        install_jobs.update_job(job_paths, journal, stage)
        emit("stage", stage="fingerprinting", message="Checking the game files before copying")
        source_facts = install_jobs.payload_identity(source, check_cancel, exclude_paths=exclude_paths,
                                                     progress=file_progress(copy_estimate))
        space_plan["sourceCopyBytesEstimate"] = source_facts["bytes"]
        check_space(job_paths, journal, root, "copying",
                    source_facts["bytes"] + install_jobs.MAX_BUILD_BYTES,
                    log_path, space_plan)
        log_event(log_path, "copying", "validated source copy started",
                  {"files": source_facts["files"], "bytes": source_facts["bytes"]})
        emit("stage", stage="copying", message="Copying your game files")
        install_jobs.copy_payload(source, staged_data, check_cancel, source_facts["bytes"],
                                  exclude_paths=exclude_paths, progress=file_progress(source_facts["bytes"]))
        emit("stage", stage="fingerprinting", message="Checking the copied files")
        copied = install_jobs.payload_identity(staged_data, check_cancel,
                                               progress=file_progress(source_facts["bytes"]))
        if copied["sha256"] != source_facts["sha256"]:
            raise SourceError("copy_validation_failed", "The staged game copy differs from its source.",
                              ["Check storage space and source permissions."], str(source))
        if source_kind in ("zip", "inno", "installshield"):
            shutil.rmtree(staging / "extracted")
        validate_game(staged_data, executable_relative, cfg["game"]["sha256"], required)
        for module in cfg["aux_modules"]:
            image = staged_data / selected["auxiliaryPaths"][module["name"]]
            if not image.is_file() or hashlib.sha256(image.read_bytes()).hexdigest() != module["sha256"]:
                raise ValueError(f"Missing or unsupported auxiliary module: {module['name']}")
        if cfg["setup"].get("downloads"):
            stage = "validating"
            install_jobs.update_job(job_paths, journal, stage)
            try:
                downloaded = acquire_dependencies(cfg, staged_data / executable_relative, staging / "downloads",
                                                  check_cancel, lambda message: emit("stage", stage=stage, message=message))
            except DependencyError as error:
                raise SourceError("dependency_download_failed", str(error),
                                  ["Check your internet connection and retry, or place the file beside "
                                   "Civ4BeyondSword.exe yourself."], str(source),
                                  {"dependency": error.label, "reason": error.reason}) from error
            if downloaded:
                copied = install_jobs.payload_identity(staged_data, check_cancel, progress=file_progress())
        journal["validatedInputs"] = {"source": source_facts, "stagedOriginal": copied,
                                      "executableSha256": cfg["game"]["sha256"]}
        log_event(log_path, "copying", "staged original validated",
                  {"files": copied["files"], "bytes": copied["bytes"], "sha256": copied["sha256"]})
        emit("stage", stage="building", message="Building the game app")
        stage = "building"
        install_jobs.update_job(job_paths, journal, stage)
        publish_dir = staging / "publish"
        if publish_dir.exists():
            remove(publish_dir)
        monitor_baseline = copied["bytes"]
        existing_growth = max(0, install_jobs.tree_size(staging, check_cancel) - monitor_baseline)
        check_space(job_paths, journal, root, "building",
                    max(0, install_jobs.MAX_BUILD_BYTES - existing_growth), log_path, space_plan)
        isolated_env = os.environ.copy()
        isolated_env["PYTHONUNBUFFERED"] = "1"
        isolated_env["RECOMP_SOURCE_EXECUTABLE"] = executable_relative.as_posix()
        isolated_env["RECOMP_SOURCE_AUX_PATHS"] = json.dumps(selected["auxiliaryPaths"], ensure_ascii=False)
        isolated_paths = {"TMPDIR": staging / "tmp", "TMP": staging / "tmp", "TEMP": staging / "tmp"}
        for key, value in isolated_paths.items():
            value.mkdir(parents=True, exist_ok=True)
            isolated_env[key] = str(value)
        log_event(log_path, "building", "build started",
                  {"stagingBaselineBytes": monitor_baseline,
                   "buildAllowanceBytes": install_jobs.MAX_BUILD_BYTES,
                   "temporaryEnvironmentRoots": {key: value.relative_to(root).as_posix()
                                                   for key, value in isolated_paths.items()}})
        run([sys.executable, str(KIT / "tools/build.py"), "--game", game_id,
             "--build-root", str(build_root), "--regenerate", "--no-emission-cache", "--no-observe",
             "--target", "app"], log_path,
            build_monitor={"root": staging, "volume": root,
                           "baselineBytes": monitor_baseline}, env=isolated_env)
        log_event(log_path, "building", "build completed")
        emit("stage", stage="packaging", message="Preparing the finished app")
        binary = artifact(cfg, build_root, installed=False)
        if not binary.is_file():
            raise ValueError("Build finished without the expected game application")
        publish_root = staging / "publish"
        publish_root.mkdir()
        built = build_root / (cfg["game"]["app_name"] + ".app") if platform.system() == "Darwin" else binary.parent
        if platform.system() == "Darwin":
            staged_app = publish_root
            built.replace(publish_root / built.name)
            configure_name = sidecar_bundle(cfg)
            configure_contents = publish_root / configure_name / "Contents"
            configure_binary = build_root / "recomp" / "ConfigureLauncher"
            if not configure_binary.is_file():
                raise ValueError("Build finished without the native Configure launcher")
            configure_executable = configure_contents / "MacOS" / "ConfigureLauncher"
            configure_executable.parent.mkdir(parents=True)
            shutil.copy2(configure_binary, configure_executable)
            configure_executable.chmod(configure_executable.stat().st_mode | 0o111)
            with (configure_contents / "Info.plist").open("wb") as plist:
                plistlib.dump({"CFBundleExecutable": "ConfigureLauncher",
                    "CFBundleIdentifier": cfg["game"]["bundle_id"] + ".configure",
                    "CFBundleName": "Configure " + cfg["game"]["name"],
                    "CFBundlePackageType": "APPL", "LSMinimumSystemVersion": "12.0"}, plist,
                    fmt=plistlib.FMT_XML)
            game_bundle = publish_root / f"{cfg['game']['app_name']}.app"
            configure_bundle = publish_root / configure_name
            from app_icons import set_bundle_icon, svg_image, write_desktop_icons
            configure_icons = build_root / "icons/configure"
            write_desktop_icons(svg_image("configure"), configure_icons)
            set_bundle_icon(configure_bundle, configure_icons / "icon.icns")
            game_executable = game_bundle / "Contents" / "MacOS" / cfg["game"]["app_name"]
            if not os.access(game_executable, os.X_OK) or not os.access(configure_executable, os.X_OK):
                raise ValueError("A published app executable is missing its executable permission")
            subprocess.run(["codesign", "--force", "--deep", "--sign", "-", str(configure_bundle)], check=True)
            for bundle in (game_bundle, configure_bundle):
                subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
                native_executable = (bundle / "Contents" / "MacOS" /
                                     (cfg["game"]["app_name"] if bundle == game_bundle else "ConfigureLauncher"))
                audit_macos_bundle(bundle, native_executable)
        else:
            staged_app = publish_root / "app"
            built.replace(staged_app)
        relative_binary = artifact(cfg, root).relative_to(root) if platform.system() == "Darwin" else artifact(cfg, root).relative_to(root / "app")
        if platform.system() == "Darwin" and not (publish_root / relative_binary).is_file():
            raise ValueError("Installation staging is missing the game application")
        check_cancel()
        current_recipe = recipe(directory)
        current_source = ({"kind": source_kind, "sha256": adapter.archive_digest(source_path, check_cancel)}
                          if source_kind in ("zip", "inno", "installshield") else
                          {"kind": "folder", **install_jobs.payload_identity(selected["contentRoot"], check_cancel,
                                                                               exclude_paths=exclude_paths)})
        if current_recipe.get("sha256") != recipe_facts.get("sha256") or current_source != source_identity:
            raise SourceError("inputs_changed_before_publish", "The source or build recipe changed during the build.",
                              ["Review the source and start a new installation."], str(source_path))
        final_original = install_jobs.payload_identity(staged_data, check_cancel)
        if final_original != copied:
            raise SourceError("staged_input_changed", "The staged game copy changed during the build.",
                              ["Retry the installation from a clean staged copy."], str(staged_data))
        validate_game(staged_data, executable_relative, cfg["game"]["sha256"], required)
        for module in cfg["aux_modules"]:
            image = staged_data / selected["auxiliaryPaths"][module["name"]]
            if not image.is_file() or hashlib.sha256(image.read_bytes()).hexdigest() != module["sha256"]:
                raise ValueError(f"Missing or unsupported auxiliary module before publication: {module['name']}")
        if platform.system() == "Darwin":
            app_names = [f"{cfg['game']['app_name']}.app", sidecar_bundle(cfg)]
            staged_apps = [publish_root / name for name in app_names]
        else:
            app_names = ["app"]
            staged_apps = [staged_app]
        if any(not path.is_dir() for path in staged_apps):
            raise ValueError("Installation staging is missing a required app or sidecar bundle")
        app_facts = {name: install_jobs.owned_tree_identity(path, check_cancel)
                     for name, path in zip(app_names, staged_apps)}
        artifact_specs = []
        for name, source_bundle in zip(app_names, staged_apps):
            destination = root / name if platform.system() == "Darwin" else root / "app"
            had_previous = destination.exists() or destination.is_symlink()
            if destination.is_symlink():
                raise install_jobs.JobError("unsafe_publication_target", f"Refusing to replace a symbolic link: {name}")
            artifact_specs.append({"destination": destination.relative_to(root).as_posix(),
                "staged": source_bundle.relative_to(root).as_posix(), "backup": name,
                "hadPrevious": had_previous,
                "previousToken": install_jobs.directory_token(destination) if had_previous else None,
                "previousIdentity": ((install_jobs.payload_identity(destination) if name == DATA_DIR
                                      else install_jobs.owned_tree_identity(destination)) if had_previous else None),
                "stagedToken": install_jobs.directory_token(source_bundle),
                "stagedIdentity": install_jobs.owned_tree_identity(source_bundle, check_cancel)})
        staged_data_token = install_jobs.directory_token(staged_data)
        if not data.exists():
            artifact_specs.append({"destination": DATA_DIR, "staged": staged_data.relative_to(root).as_posix(),
                "backup": DATA_DIR, "hadPrevious": False, "previousToken": None,
                "previousIdentity": None,
                "stagedToken": staged_data_token, "stagedIdentity": copied})
        stage = "publishing"
        emit("stage", stage=stage, message="Saving the finished installation")
        install_jobs.update_job(job_paths, journal, stage)
        check_space(job_paths, journal, root, "publishing", 0, log_path, space_plan)
        log_event(log_path, "publishing", "staged app and original validated",
                  {"appSha256": {name: fact["sha256"] for name, fact in app_facts.items()},
                   "originalSha256": copied["sha256"]})
        facts = installed_manifest(cfg, game_id, job_identifier, executable_relative,
                                   selected.get("dataRoot", "."), source_identity,
                                   recipe_facts, app_facts, copied, journal["log"],
                                   selected["executable"].get("size", 0), staged_data,
                                   selected["auxiliaryPaths"])
        if job_paths["backup"].exists():
            install_jobs.remove_owned(job_paths["backup"], job_paths["meta"], job_identifier,
                                      journal.get("ownedTokens", {}).get("backup"))
        _, backup_token = install_jobs.create_backup(job_paths, job_identifier)
        journal.setdefault("ownedTokens", {})["backup"] = backup_token
        allowed_destinations = [item["destination"] for item in artifact_specs]
        manifest_digest = manifest_identity(facts)
        current_manifest_path = root / ".recomp-toolkit" / "installation.json"
        if current_manifest_path.is_symlink():
            raise install_jobs.JobError("unsafe_manifest_path", "The installation manifest is a symbolic link.")
        previous_manifest = (read_installation_manifest(root)
                             if current_manifest_path.is_file() and not current_manifest_path.is_symlink() else None)
        install_jobs.update_job(job_paths, journal, stage, ownedTokens=journal["ownedTokens"],
                                publication={"artifacts": artifact_specs, "allowedDestinations": allowed_destinations,
                                             "originalIdentity": copied, "appIdentity": app_facts,
                                             "manifest": facts, "manifestIdentity": manifest_digest,
                                             "previousManifestIdentity": manifest_identity(previous_manifest) if previous_manifest else None})
        for spec in artifact_specs:
            destination = root / spec["destination"]
            staged = root / spec["staged"]
            backup = job_paths["backup"] / spec["backup"]
            destination.parent.mkdir(parents=True, exist_ok=True)
            if spec["hadPrevious"]:
                destination.replace(backup)
                install_jobs._sync_directory(destination.parent)
                install_jobs._sync_directory(backup.parent)
            staged.replace(destination)
            install_jobs._sync_directory(staged.parent)
            install_jobs._sync_directory(destination.parent)
        install_jobs.write_json(root / ".recomp-toolkit" / "installation.json", facts)
        committed = True
        log_event(log_path, "publishing", "installation manifest committed",
                  {"jobId": job_identifier, "gameId": game_id})
        try:
            registry_dir = install_jobs.registry_game_dir(storage, game_id)
            registry_dir.mkdir(parents=True, exist_ok=True)
            registry_path = registry_dir / "registry.json"
            registry = install_jobs.read_versioned(registry_path, "registry") if registry_path.is_file() else {
                "schema": install_jobs.SCHEMA, "gameId": game_id, "activeJobs": []}
            registry.update(destination=str(root), gameId=game_id)
            install_jobs.write_json(registry_path, registry)
        except (OSError, ValueError) as error:
            emit("log", text=f"Installed, but library registration needs repair: {error}")
        cleanup_complete = False
        try:
            stage = "cleaning"
            emit("stage", stage=stage, message="Removing temporary build files")
            install_jobs.update_job(job_paths, journal, stage)
            log_event(log_path, stage, "owned staging cleanup started")
            install_jobs.remove_owned(job_paths["staging"], job_paths["meta"], job_identifier,
                                      journal.get("ownedTokens", {}).get("staging"))
            if job_paths["backup"].exists():
                install_jobs.remove_owned(job_paths["backup"], job_paths["meta"], job_identifier,
                                          journal.get("ownedTokens", {}).get("backup"))
            cleanup_complete = True
            log_event(log_path, stage, "owned staging cleanup completed")
        except (OSError, ValueError) as error:
            log_event(log_path, "cleaning", "cleanup needs attention", {"message": str(error)}, failure=True)
            emit("log", text=f"Published installation; cleanup needs attention: {error}")
        if cleanup_complete:
            try:
                install_jobs.update_job(job_paths, journal, "complete", validatedOutputs={"installed": facts}, publication=None)
                emit("stage", stage="complete", message="Installation complete")
            except OSError as error:
                emit("log", text=f"Published installation; job journal cleanup needs attention: {error}")
        else:
            emit("stage", stage="cleaning", message="Installation complete; cleanup will be retried")
        return {"game": game_id, "state": "ready", "jobId": job_identifier}
    except BaseException as error:
        diagnostic = error_diagnostic(error, stage, root, log_path)
        diagnostic.setdefault("details", {})["jobId"] = job_identifier
        try:
            error.diagnostic = diagnostic
        except AttributeError:
            pass
        log_event(log_path, stage, "installation attempt failed", diagnostic, failure=True)
        if not committed:
            if journal.get("publication"):
                try:
                    journal = recover_publication(root, job_identifier)
                    committed = journal.get("stage") == "complete"
                except (OSError, ValueError, KeyError) as recovery_error:
                    emit("log", text=f"Publication recovery is required: {recovery_error}")
        if not committed:
            failure_stage = "cancelled" if diagnostic["code"] == "cancelled" else "failed"
            try:
                install_jobs.update_job(job_paths, journal, failure_stage,
                                        errors=[*journal.get("errors", []), diagnostic])
            except OSError as journal_error:
                emit("log", text=f"Could not persist the failure journal: {journal_error}")
        raise


def inspect_request(request):
    validate_inspection_request(request)
    game_id = request["game"]
    directory = KIT / "games" / game_id
    if directory.parent != KIT / "games" or not (directory / "game.toml").is_file() or game_id == "stub":
        raise ValueError("Unknown game")
    cfg = game_config.load(directory)
    return inspect_source(cfg, request["source"])


def discover_request(request):
    validate_inspection_request(request)
    game_id = request["game"]
    directory = KIT / "games" / game_id
    if directory.parent != KIT / "games" or not (directory / "game.toml").is_file() or game_id == "stub":
        raise ValueError("Unknown game")
    cfg = game_config.load(directory)
    host = platform.system()
    platform_name = "windows" if host == "Windows" else "macos" if host == "Darwin" else "linux"
    roots, diagnostics, expansion_entries = expand_discovery_roots(
        platform_name, Path.home(), os.environ, check_cancel, cfg["launcher"]["install_names"])
    budget = [expansion_entries, [0, MAX_DISCOVERY_HASH_BYTES], 0]
    candidates = []
    seen = set()
    candidate_keys = set()
    skipped_symlinks = 0
    for root in roots:
        check_cancel()
        if budget[0] > MAX_DISCOVERY_ENTRIES:
            break
        try:
            if not root.exists():
                continue
            canonical = root.resolve(strict=True)
            info = canonical.stat()
            identity = (info.st_dev, info.st_ino)
            if identity in seen:
                continue
            seen.add(identity)
            if not canonical.is_dir():
                continue
            result = inspect_folder(cfg, canonical, check_cancel, discovery_budget=budget)
            for diagnostic in result["diagnostics"]:
                if diagnostic["code"] == "symlink_skipped":
                    skipped_symlinks += 1
                else:
                    diagnostics.append(diagnostic)
            for candidate in result["candidates"]:
                candidate_key = (os.path.normcase(str(Path(candidate["contentRoot"]).resolve(strict=True))),
                                 os.path.normcase(candidate["executable"]["path"]),
                                 candidate["executable"].get("sha256"))
                if candidate_key in candidate_keys:
                    continue
                candidate_keys.add(candidate_key)
                candidate["discoveryRoot"] = str(canonical)
                candidates.append(candidate)
            if budget[0] >= MAX_DISCOVERY_ENTRIES or budget[2] >= MAX_DISCOVERY_CANDIDATES:
                break
        except OSError as error:
            diagnostics.append({"code": "discovery_path_unavailable", "stage": "inspecting",
                                "path": str(root), "message": "A configured game-search location could not be read.",
                                "details": {"error": str(error)},
                                "recovery": ["Check the search location permissions."]})
    if skipped_symlinks:
        diagnostics.append({"code": "symlink_skipped", "stage": "inspecting", "path": ".",
                            "message": "Symbolic links were skipped at configured search locations.",
                            "details": {"count": skipped_symlinks},
                            "recovery": ["Choose a source without symbolic links."]})
    scan_complete = not any(item["code"] in ("search_limit", "candidate_limit", "hash_limit",
                                              "required_artifact_limit", "inaccessible_path",
                                              "path_collision", "symlink_skipped", "unsafe_path",
                                              "discovery_entry_limit", "discovery_root_limit",
                                              "discovery_path_unavailable", "steam_library_file_limit",
                                              "steam_library_file_unavailable") for item in diagnostics)
    return {"schema": {"major": 1, "minor": 0}, "source": {"kind": "discovery"},
            "candidates": candidates, "diagnostics": diagnostics,
            "limits": {"maxDiscoveryRoots": MAX_DISCOVERY_ROOTS,
                       "maxDiscoveryEntries": MAX_DISCOVERY_ENTRIES,
                       "maxDiscoveryCandidates": MAX_DISCOVERY_CANDIDATES,
                       "maxDiscoveryHashBytes": MAX_DISCOVERY_HASH_BYTES,
                       "rootsVisited": len(seen), "entriesSeen": budget[0],
                       "candidatesInspected": budget[2], "hashBytes": budget[1][0]},
            "scanComplete": scan_complete}


def preflight_request(request):
    source = Path(request.get("source", "")).expanduser().resolve(strict=True)
    destination = Path(request.get("destination", "")).expanduser().resolve()
    volume = destination
    while not volume.exists():
        volume = volume.parent
    if not volume.is_dir():
        raise install_jobs.JobError("preflight_path_invalid", "Choose a destination inside a folder.")
    source_kind = request.get("sourceKind")
    if source_kind == "zip":
        source_bytes = zip_adapter.archive_uncompressed_size(source, check_cancel)
        destination_copy_bytes = source_bytes * 2
    elif source_kind in ("inno", "installshield"):
        validate_inspection_request(request)
        directory = KIT / "games" / request["game"]
        if directory.parent != KIT / "games" or not (directory / "game.toml").is_file():
            raise ValueError("Unknown game")
        adapter = archive_adapter(game_config.load(directory), source_kind)
        source_bytes = adapter.archive_uncompressed_size(source, check_cancel)
        destination_copy_bytes = source_bytes * 2 + adapter.source_size(source)
    elif source.is_dir():
        source_bytes = install_jobs.payload_size(source, check_cancel)["bytes"]
        destination_copy_bytes = source_bytes
    else:
        raise install_jobs.JobError("preflight_source_unsupported", "The selected source has no bounded space estimate.")
    required = (destination_copy_bytes + install_jobs.MAX_BUILD_BYTES + install_jobs.SAFETY_RESERVE_BYTES
                + install_jobs.MAX_ATTEMPT_LOG_BYTES)
    destination_space = shutil.disk_usage(volume)
    source_space = shutil.disk_usage(source)
    return {"source": {"path": str(source), "bytes": source_bytes, "freeBytes": source_space.free},
            "destination": {"path": str(destination), "freeBytes": destination_space.free,
                            "requiredBytes": required, "enough": destination_space.free >= required}}


def adopt_request(request, storage):
    game_id = request.get("game")
    directory = KIT / "games" / str(game_id)
    root_input = Path(request.get("destination", "")).expanduser()
    root = root_input.resolve(strict=True)
    if root_input != root or not root.is_dir():
        raise install_jobs.JobError("unsafe_adoption_path", "Choose the final game folder itself.")
    if directory.parent != KIT / "games" or game_id == "stub" or not (directory / "game.toml").is_file():
        raise install_jobs.JobError("unknown_game", "The requested game is unavailable.")
    cfg = game_config.load(directory, root)
    registry_dir = install_jobs.registry_game_dir(storage, game_id)
    with install_jobs.DestinationLock(storage, lock_name="registry.lock"):
        with install_jobs.DestinationLock(root):
            manifest_path = root / ".recomp-toolkit" / "installation.json"
            manifest = read_installation_manifest(root)
            validate_installed_manifest(cfg, root, manifest)
            registry_path = registry_dir / "registry.json"
            registry = (install_jobs.read_versioned(registry_path, "registry")
                        if registry_path.is_file() and not registry_path.is_symlink()
                        else {"schema": install_jobs.SCHEMA, "gameId": game_id, "activeJobs": []})
            if registry.get("gameId") != game_id or registry.get("schema", {}).get("major") != install_jobs.SCHEMA["major"]:
                raise install_jobs.JobError("invalid_registry", "The existing library record has an unsupported identity.")
            registry["destination"] = str(root)
            registry_dir.mkdir(parents=True, exist_ok=True)
            install_jobs.write_json(registry_path, registry)
    return {"game": game_id, "destination": str(root), "state": "ready"}


def validate_inspection_request(request):
    schema = request.get("schema")
    if schema is None:
        return
    if (not isinstance(schema, dict) or type(schema.get("major")) is not int
            or type(schema.get("minor")) is not int or schema["minor"] < 0):
        raise SourceError("invalid_request_schema", "The source request has an invalid schema version.",
                          ["Update the library client and retry the inspection."])
    if schema["major"] != 1:
        raise SourceError("unsupported_request_schema", "The source request uses an unsupported schema version.",
                          ["Update the library client to use a supported source request version."],
                          details={"received": schema, "supportedMajor": 1})


def delete_job(request, storage):
    game_id = request.get("game")
    identifier = install_jobs.job_id(request.get("jobId"))
    registry_dir = install_jobs.registry_game_dir(storage, game_id)
    with install_jobs.DestinationLock(storage, lock_name="registry.lock"):
        activity_path = registry_dir / "activity.json"
        activity = install_jobs.read_versioned(activity_path, "activity")
        locator = next((item for item in activity.get("activeJobs", [])
                        if isinstance(item, dict) and item.get("jobId") == identifier), None)
        if not locator:
            raise install_jobs.JobError("unknown_job", "The requested job is not registered.")
        destination_input = Path(locator["destination"])
        destination = destination_input.resolve(strict=True)
        if destination_input != destination or not destination.is_dir():
            raise install_jobs.JobError("unsafe_job_destination", "The saved job destination is not canonical.")
        with install_jobs.DestinationLock(destination):
            job_paths, journal = install_jobs.load_job(destination, identifier)
            if journal.get("gameId") != game_id or journal.get("publication") or journal.get("stage") in ("publishing", "cleaning", "complete"):
                raise install_jobs.JobError("job_requires_recovery", "This job cannot be deleted before publication recovery.")
            install_jobs.update_job(job_paths, journal, "cancelled")
            for key in ("staging", "backup"):
                if job_paths[key].exists():
                    install_jobs.remove_owned(job_paths[key], job_paths["meta"], identifier,
                                              journal.get("ownedTokens", {}).get(key))
            install_jobs.update_job(job_paths, journal, "cancelled")
            activity["activeJobs"] = [item for item in activity["activeJobs"] if item != locator]
            install_jobs.write_json(activity_path, activity)
        for directory in (job_paths["jobs"], job_paths["logs"], job_paths["meta"], destination):
            try:
                directory.rmdir()
            except OSError:
                pass
    return {"deleted": identifier}


def resume_install(request, storage):
    validate_inspection_request(request)
    game_id = request["game"]
    directory = KIT / "games" / game_id
    if directory.parent != KIT / "games" or game_id == "stub" or not (directory / "game.toml").is_file():
        raise install_jobs.JobError("unknown_game", "The requested game is unavailable.")
    source_path = Path(request["source"]).expanduser().resolve(strict=True)
    cfg = game_config.load(directory)
    inspection = inspect_source(cfg, source_path)
    if inspection["source"]["kind"] in ("folder", "executable"):
        selected = select_candidate(inspection, request.get("candidateId"),
                                    source_path if inspection["source"]["kind"] == "executable" else None)
        source_root = Path(selected["contentRoot"]).resolve()
        if Path(storage).expanduser().resolve().is_relative_to(source_root):
            raise SourceError("storage_source_overlap",
                              "The library storage folder is inside the selected game source.",
                              ["Choose storage outside the source folder before importing."],
                              str(storage), {"source": str(source_root)})
    destination_value = request.get("destination")
    if not destination_value:
        game_registry_dir = install_jobs.registry_game_dir(storage, game_id)
        activity_path = game_registry_dir / "activity.json"
        activity = install_jobs.read_versioned(activity_path, "activity") if activity_path.is_file() else {}
        for locator in activity.get("activeJobs", []):
            if isinstance(locator, dict) and locator.get("jobId") == request.get("jobId"):
                destination_value = locator.get("destination")
                break
    destination = Path(destination_value or installation_root(storage, game_id)).expanduser().resolve()
    request = {**request, "destination": str(destination)}
    with install_jobs.DestinationLock(storage, lock_name="registry.lock") as registry_lock:
        with install_jobs.DestinationLock(destination) as destination_lock:
            global DESTINATION_LOCKS
            DESTINATION_LOCKS = [registry_lock, destination_lock]
            try:
                _, journal = install_jobs.load_job(destination, request.get("jobId"))
                if journal.get("gameId") != game_id:
                    raise install_jobs.JobError("job_game_mismatch", "The saved job belongs to a different game.")
                if journal.get("publication"):
                    recover_publication(destination, request["jobId"])
                prepared = prepare_install(request, storage)
                if prepared["root"] != destination_lock.root:
                    raise install_jobs.JobError("destination_changed", "The registered destination changed during preflight.")
                return install(request, storage, prepared)
            finally:
                DESTINATION_LOCKS = []


def play(request, storage):
    game_id = request["game"]
    game_dir = KIT / "games" / game_id
    if game_dir.parent != KIT / "games" or game_id == "stub" or not (game_dir / "game.toml").is_file():
        raise install_jobs.JobError("unknown_game", "The requested game is unavailable.")
    root = installation_root(storage, game_id)
    cfg = game_config.load(game_dir, root)
    saved = read_installation_manifest(root)
    image = validate_installed_manifest(cfg, root, saved)
    expected_launch = saved["launch"]
    binary = artifact(cfg, root)
    log = root / "game.log"
    data_root = root / saved.get("paths", {}).get("dataRoot", DATA_DIR)
    return {"game": game_id, "log": str(log), "binary": str(binary), "cwd": str(data_root)}


def main():
    global CANCEL_FILE, DESTINATION_LOCKS
    parser = argparse.ArgumentParser()
    parser.add_argument("--storage", type=Path, required=True)
    parser.add_argument("--request", required=True)
    args = parser.parse_args()
    os.environ.pop("RECOMP_BUILD_ROOT", None)
    os.environ.pop("BUILDLOCK_HELD", None)
    request = json.loads(args.request)
    if request.get("action") in ("install", "inspect", "discover", "preflight"):
        CANCEL_FILE = Path(request["cancelFile"]) if request.get("cancelFile") else None
        if os.name != "nt":
            signal.signal(signal.SIGTERM, cancel)
    storage = args.storage.expanduser().resolve()
    try:
        action = request["action"]
        if action == "catalog":
            result = {"games": catalog(storage), "storage": str(storage)}
        elif action == "inspect":
            result = inspect_request(request)
        elif action == "discover":
            result = discover_request(request)
        elif action == "preflight":
            result = preflight_request(request)
        elif action == "adopt":
            result = adopt_request(request, storage)
        elif action == "job_log":
            result = job_log(request, storage)
        elif action == "delete_job":
            result = delete_job(request, storage)
        elif action == "check":
            result = prerequisites(request.get("needsRust", True), request.get("needsInnoextract", False), request.get("needsUnshield", False))
        elif action == "install":
            emit("stage", stage="checking", message="Checking your copy and install location")
            if request.get("jobId"):
                result = resume_install(request, storage)
            else:
                prepared = prepare_install(request, storage)
                with install_jobs.DestinationLock(storage, lock_name="registry.lock") as registry_lock:
                    with install_jobs.DestinationLock(prepared["root"]) as destination_lock:
                        DESTINATION_LOCKS = [registry_lock, destination_lock]
                        try:
                            prepared = prepare_install(request, storage)
                            if prepared["root"] != destination_lock.root:
                                raise install_jobs.JobError("destination_changed", "The registered destination changed during preflight.")
                            result = install(request, storage, prepared)
                        finally:
                            DESTINATION_LOCKS = []
        elif action == "play":
            root = installation_root(storage, request["game"])
            with install_jobs.DestinationLock(root):
                launch = play(request, storage)
            with Path(launch["log"]).open("ab") as output:
                subprocess.Popen([launch["binary"]], cwd=launch["cwd"], stdout=output, stderr=output,
                                 start_new_session=os.name != "nt")
            result = {"game": launch["game"], "log": launch["log"]}
        else:
            raise ValueError("Unknown library action")
        emit("result", result=result)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, subprocess.TimeoutExpired, TimeoutError) as error:
        fields = {"message": str(error)}
        if getattr(error, "diagnostic", None):
            fields["diagnostic"] = error.diagnostic
        elif isinstance(error, InterruptedError):
            fields["diagnostic"] = {"code": "cancelled", "stage": "cancelled", "path": ".",
                                     "message": "The installation was cancelled.", "details": {},
                                     "recovery": ["resume", "choose_source"]}
        elif isinstance(error, install_jobs.JobError):
            fields["diagnostic"] = {"code": error.code, "stage": "installing", "path": ".",
                                     "message": str(error), "details": error.details,
                                     "recovery": error.recovery}
        else:
            fields["diagnostic"] = {"code": "library_operation_failed", "stage": request.get("action", "unknown"),
                                     "path": ".", "message": str(error), "details": {},
                                     "recovery": ["check_permissions", "open_log", "retry"]}
        emit("error", **fields)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
