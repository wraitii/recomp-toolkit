import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import time
import unicodedata
import uuid


SCHEMA = {"major": 1, "minor": 0}
MAX_PAYLOAD_FILES = 125000
MAX_PAYLOAD_BYTES = 8 * 1024 * 1024 * 1024
MAX_RECIPE_FILES = 200000
MAX_RECIPE_BYTES = 4 * 1024 * 1024 * 1024
MAX_PATH_DEPTH = 64
MAX_PATH_BYTES = 4096
HASH_CHUNK = 1024 * 1024
MAX_BUILD_BYTES = 10 * 1024 * 1024 * 1024
SAFETY_RESERVE_BYTES = 1024 * 1024 * 1024
MAX_ATTEMPT_LOG_BYTES = 16 * 1024 * 1024
MAX_LOG_FAILURE_RESERVE = 64 * 1024
MAX_RETAINED_LOGS = 8
MAX_RETAINED_LOG_BYTES = 64 * 1024 * 1024
MAX_MONITOR_ENTRIES = 200000
MAX_MONITOR_DEPTH = 128
RECIPE_DIRS = ("runtime", "dx", "host", "platform", "mods", "graphics", "cmake", "third_party")
RECIPE_FILES = ("CMakeLists.txt", "CMakePresets.json")
RECIPE_TOOL_EXCLUDES = {"build_library.py", "check_game_literals.py",
                        "check_repo.py", "format.py", "install_jobs.py", "ios_logs.py",
                        "setup.py", "source_inspection.py", "test.py", "zip_adapter.py", "inno_adapter.py", "unshield_adapter.py"}
IGNORED_PARTS = {".git", "__pycache__", ".pytest_cache", "tests", "test", ".venv"}
JOURNAL_STAGES = {"inspecting", "discovering", "preparing", "extracting", "copying", "validating",
                  "building", "publishing", "cleaning", "complete", "failed", "cancelled"}


class JobError(ValueError):
    def __init__(self, code, message, recovery=None, details=None):
        super().__init__(message)
        self.code = code
        self.recovery = recovery or []
        self.details = details or {}


def process_start_time(pid=None):
    pid = os.getpid() if pid is None else pid
    if platform.system() == "Linux":
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().split()
            boot = next(int(line.split()[1]) for line in Path("/proc/stat").read_text().splitlines()
                        if line.startswith("btime "))
            ticks = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
            return boot + int(fields[21]) / ticks
        except (OSError, StopIteration, ValueError, IndexError):
            return None
    if platform.system() == "Darwin":
        try:
            value = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)],
                                   capture_output=True, text=True, timeout=2, check=True).stdout.strip()
            return time.mktime(time.strptime(value, "%a %b %d %H:%M:%S %Y")) if value else None
        except (OSError, subprocess.SubprocessError, ValueError):
            return None
    return None


def job_id(value=None):
    value = str(uuid.uuid4()) if value is None else str(value)
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError) as error:
        raise JobError("invalid_job_id", "The installation job ID is invalid.") from error
    if str(parsed) != value:
        raise JobError("invalid_job_id", "The installation job ID is not in canonical form.")
    return value


def destination_paths(destination):
    root = Path(destination).expanduser().resolve()
    meta = root / ".recomp-toolkit"
    if meta.is_symlink():
        raise JobError("unsafe_metadata_path", "The installation metadata folder is a symbolic link.")
    return root, meta


def registry_game_dir(storage, game_id):
    base = Path(storage).expanduser().resolve()
    directory = base / game_id
    if directory.is_symlink() or directory.resolve().parent != base:
        raise JobError("unsafe_registry_path", "The game registry folder is a symbolic link or escapes storage.")
    return directory


class DestinationLock:
    def __init__(self, destination, blocking=False, lock_name="install.lock"):
        self.root, self.meta = destination_paths(destination)
        if Path(lock_name).name != lock_name:
            raise ValueError("invalid lock name")
        self.path = self.meta / lock_name
        self.blocking = blocking
        self.fd = None

    def __enter__(self):
        self.root.mkdir(parents=True, exist_ok=True)
        if self.meta.is_symlink():
            raise JobError("unsafe_metadata_path", "The installation metadata folder is a symbolic link.")
        self.meta.mkdir(parents=True, exist_ok=True)
        jobs = self.meta / "jobs"
        logs = self.meta / "logs"
        if (jobs.is_symlink() or (jobs.exists() and not jobs.is_dir())
                or logs.is_symlink() or (logs.exists() and not logs.is_dir())):
            raise JobError("unsafe_metadata_path", "The installation job metadata path is unsafe.")
        flags = os.O_CREAT | os.O_RDWR
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            self.fd = os.open(self.path, flags, 0o600)
            if not stat.S_ISREG(os.fstat(self.fd).st_mode):
                raise JobError("unsafe_lock_path", "The installation lock is not a regular file.")
            if os.name == "nt":
                import msvcrt
                os.lseek(self.fd, 0, os.SEEK_SET)
                msvcrt.locking(self.fd, msvcrt.LK_LOCK if self.blocking else msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(self.fd, fcntl.LOCK_EX | (0 if self.blocking else fcntl.LOCK_NB))
        except BlockingIOError as error:
            self.close()
            raise JobError("destination_busy", "Another library job is using this destination.",
                           ["Wait for the other job or choose another destination."]) from error
        except BaseException:
            self.close()
            raise
        owner = json.dumps({"pid": os.getpid(), "startTime": process_start_time()}, separators=(",", ":")).encode()
        os.lseek(self.fd, 0, os.SEEK_SET)
        os.ftruncate(self.fd, 0)
        os.write(self.fd, owner)
        os.fsync(self.fd)
        return self

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def __exit__(self, exc_type, exc, tb):
        if self.fd is not None and os.name == "nt":
            import msvcrt
            os.lseek(self.fd, 0, os.SEEK_SET)
            try:
                msvcrt.locking(self.fd, msvcrt.LK_UNLCK, 1)
            except OSError:
                pass
        self.close()
        return False


def _sync_directory(path):
    if os.name == "nt":
        return
    try:
        fd = os.open(path, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    except OSError:
        raise


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f".{uuid.uuid4()}.part")
    data = (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    fd = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        _sync_directory(path.parent)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def append_job_log(path, stage, event, details=None, failure=False):
    path = Path(path)
    if path.parent.is_symlink():
        raise JobError("unsafe_log_path", "The job log folder is a symbolic link.")
    path.parent.mkdir(parents=True, exist_ok=True)
    record = {"time": int(time.time()), "stage": str(stage)[:128],
              "event": str(event)[:2048], "details": details or {}}
    data = (json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n").encode("ascii")
    flags = os.O_CREAT | os.O_APPEND | os.O_WRONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(path, flags, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise JobError("unsafe_log_path", "The job log is not a regular file.")
        size = os.fstat(fd).st_size
        limit = MAX_ATTEMPT_LOG_BYTES if failure else MAX_ATTEMPT_LOG_BYTES - MAX_LOG_FAILURE_RESERVE
        remaining = limit - size
        if remaining <= 0:
            return False
        if len(data) > remaining:
            if failure:
                fallback = {"time": int(time.time()), "stage": str(stage)[:128],
                            "event": "failure event truncated", "details": {}}
                compact = (json.dumps(fallback, ensure_ascii=True, separators=(",", ":")) + "\n").encode("ascii")
                if len(compact) <= remaining:
                    data = compact
                else:
                    return False
            else:
                return False
        with os.fdopen(fd, "ab", closefd=False) as stream:
            stream.write(data)
            stream.flush()
        return True
    finally:
        os.close(fd)


def disk_space(root, required_bytes, phase, plan=None):
    root = Path(root).resolve()
    usage = shutil.disk_usage(root)
    if usage.free < required_bytes:
        raise JobError("insufficient_space", "The destination volume does not have enough free space for the next install stage.",
                       ["Free space on the destination volume and retry."],
                       {"path": str(root), "phase": phase, "freeBytes": usage.free,
                        "requiredBytes": required_bytes, "safetyReserveBytes": SAFETY_RESERVE_BYTES,
                        "plan": plan or {}})
    return {"path": str(root), "phase": phase, "freeBytes": usage.free,
            "requiredBytes": required_bytes, "checkedAt": int(time.time())}


def tree_size(root, cancel=None):
    root = Path(root).resolve(strict=True)
    total = 0
    entries = 0
    stack = [(root, 0)]
    while stack:
        directory, depth = stack.pop()
        if depth > MAX_MONITOR_DEPTH:
            raise JobError("build_monitor_depth_limit", "The staged build tree is nested too deeply to monitor safely.")
        try:
            children = os.scandir(directory)
        except FileNotFoundError:
            continue
        with children:
            for child in children:
                if cancel:
                    cancel()
                entries += 1
                if entries > MAX_MONITOR_ENTRIES:
                    raise JobError("build_monitor_entry_limit", "The staged build tree contains too many entries to monitor safely.")
                try:
                    info = child.stat(follow_symlinks=False)
                except FileNotFoundError:
                    continue
                if stat.S_ISDIR(info.st_mode):
                    stack.append((Path(child.path), depth + 1))
                elif stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
                    total += info.st_size
                else:
                    raise JobError("build_monitor_special_file", "The staged build tree contains an unsupported special file.",
                                   details={"path": child.path})
    return total


def read_versioned(path, kind):
    path = Path(path)
    if path.is_symlink():
        raise JobError(f"unsafe_{kind}_path", f"The {kind} record is a symbolic link.")
    try:
        value = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise JobError(f"invalid_{kind}", f"The {kind} record cannot be read safely.",
                       ["Keep the record and its files for review."]) from error
    schema = value.get("schema") if isinstance(value, dict) else None
    if (not isinstance(schema, dict) or type(schema.get("major")) is not int
            or type(schema.get("minor")) is not int or schema["minor"] < 0):
        raise JobError(f"invalid_{kind}_schema", f"The {kind} record has an invalid schema.",
                       ["Keep the record and its files for review."])
    if schema["major"] != SCHEMA["major"]:
        raise JobError(f"unsupported_{kind}_schema", f"The {kind} record uses an unsupported schema version.",
                       ["Update the library before resuming this installation."],
                       {"received": schema, "supportedMajor": SCHEMA["major"]})
    return value


def paths(destination, identifier):
    identifier = job_id(identifier)
    root, meta = destination_paths(destination)
    jobs = meta / "jobs"
    if meta.is_symlink() or jobs.is_symlink() or (meta.exists() and not meta.is_dir()) or (jobs.exists() and not jobs.is_dir()):
        raise JobError("unsafe_metadata_path", "The installation job metadata path is a symbolic link.")
    return {"root": root, "meta": meta, "jobs": jobs, "journal": jobs / f"{identifier}.json",
            "staging": meta / f"staging-{identifier}", "backup": meta / f"backup-{identifier}",
            "logs": meta / "logs"}


def _check_owned_dir(path, expected_parent, identifier, create=False, token=None):
    path = Path(path)
    expected_parent = Path(expected_parent).resolve()
    if path.parent.resolve() != expected_parent or path.is_symlink():
        raise JobError("unsafe_job_path", "A job path is outside its metadata folder or is a symbolic link.")
    if create:
        path.mkdir(parents=False, exist_ok=False)
        info = path.stat()
        write_json(path / "owner.json", {"schema": SCHEMA, "jobId": identifier,
                                         "device": info.st_dev, "inode": info.st_ino})
        _sync_directory(expected_parent)
    else:
        info = path.stat()
        marker_path = path / "owner.json"
        if not marker_path.exists() and token:
            if token.get("device") != info.st_dev or token.get("inode") != info.st_ino:
                raise JobError("job_owner_mismatch", "The job folder no longer matches its journaled identity.")
        else:
            marker = read_versioned(marker_path, "job_owner")
            if (marker.get("jobId") != identifier or not stat.S_ISDIR(info.st_mode)
                    or marker.get("device") != info.st_dev or marker.get("inode") != info.st_ino):
                raise JobError("job_owner_mismatch", "The job folder ownership marker does not match its journal.")
        if token and (token.get("device") != info.st_dev or token.get("inode") != info.st_ino):
            raise JobError("job_owner_mismatch", "The job folder does not match its journaled identity.")
    return path


def create_job(destination, game_id, source, source_identity, recipe_identity, candidate_id, attempt=1):
    identifier = job_id()
    job_paths = paths(destination, identifier)
    job_paths["meta"].mkdir(parents=True, exist_ok=True)
    jobs = job_paths["journal"].parent
    if jobs.is_symlink():
        raise JobError("unsafe_metadata_path", "The jobs folder is a symbolic link.")
    jobs.mkdir(parents=True, exist_ok=True)
    _check_owned_dir(job_paths["staging"], job_paths["meta"], identifier, create=True)
    staging_token = directory_token(job_paths["staging"])
    journal = {"schema": SCHEMA, "jobId": identifier, "gameId": game_id,
               "destination": str(job_paths["root"]), "source": source,
               "sourceIdentity": source_identity, "recipeIdentity": recipe_identity,
               "candidateId": candidate_id, "attempt": attempt,
               "stage": "inspecting", "validatedInputs": {}, "validatedOutputs": {},
               "errors": [], "log": f".recomp-toolkit/logs/{identifier}-{attempt}.log",
               "owned": {"staging": f"staging-{identifier}", "backup": f"backup-{identifier}"},
               "ownedTokens": {"staging": staging_token},
               "createdAt": int(time.time()), "updatedAt": int(time.time())}
    write_json(job_paths["journal"], journal)
    return job_paths, journal


def load_job(destination, identifier):
    job_paths = paths(destination, identifier)
    journal = read_versioned(job_paths["journal"], "job")
    if journal.get("jobId") != identifier or Path(journal.get("destination", "")).resolve() != job_paths["root"]:
        raise JobError("job_identity_mismatch", "The job journal does not match this destination.")
    if journal.get("stage") not in JOURNAL_STAGES:
        raise JobError("invalid_job_stage", "The job journal has an unknown stage.")
    if not isinstance(journal.get("owned"), dict):
        raise JobError("invalid_job_ownership", "The job journal has no valid ownership record.")
    for key, expected in (("staging", f"staging-{identifier}"), ("backup", f"backup-{identifier}")):
        if journal["owned"].get(key) != expected:
            raise JobError("invalid_job_ownership", "The job journal contains an unexpected owned path.")
    if job_paths["staging"].exists():
        _check_owned_dir(job_paths["staging"], job_paths["meta"], identifier,
                         token=journal.get("ownedTokens", {}).get("staging"))
    elif journal.get("stage") not in {"failed", "cancelled", "publishing", "cleaning", "complete"}:
        raise JobError("missing_job_staging", "The job staging folder is missing.")
    if job_paths["backup"].exists():
        _check_owned_dir(job_paths["backup"], job_paths["meta"], identifier,
                         token=journal.get("ownedTokens", {}).get("backup"))
    return job_paths, journal


def update_job(job_paths, journal, stage, **fields):
    journal.update(fields)
    journal["stage"] = stage
    journal["updatedAt"] = int(time.time())
    write_json(job_paths["journal"], journal)


def remove_owned(path, parent, identifier, token=None):
    path = _check_owned_dir(path, parent, identifier, token=token)
    identity = directory_token(path)
    marker = path / "owner.json"

    def clear(directory, depth=0):
        if depth > MAX_PATH_DEPTH:
            raise JobError("job_cleanup_depth_limit", "The job-owned folder is nested too deeply to clean safely.")
        with os.scandir(directory) as entries:
            for entry in entries:
                child = Path(entry.path)
                if child == marker:
                    continue
                if entry.is_symlink():
                    child.unlink()
                    continue
                if entry.is_dir(follow_symlinks=False):
                    clear(child, depth + 1)
                    child.rmdir()
                elif entry.is_file(follow_symlinks=False):
                    child.unlink()
                else:
                    raise JobError("unsafe_job_contents", "A job-owned folder contains a special file.")

    clear(path)
    marker.unlink(missing_ok=True)
    try:
        path.rmdir()
    except OSError:
        info = path.stat()
        if info.st_dev == identity.get("device") and info.st_ino == identity.get("inode"):
            write_json(marker, {"schema": SCHEMA, "jobId": identifier,
                                "device": info.st_dev, "inode": info.st_ino})
            _sync_directory(path)
        raise
    _sync_directory(parent)


def create_backup(job_paths, identifier):
    path = _check_owned_dir(job_paths["backup"], job_paths["meta"], identifier, create=True)
    return path, directory_token(path)


def ensure_metadata(destination):
    root, meta = destination_paths(destination)
    root.mkdir(parents=True, exist_ok=True)
    if meta.is_symlink():
        raise JobError("unsafe_metadata_path", "The installation metadata folder is a symbolic link.")
    meta.mkdir(exist_ok=True)
    jobs = meta / "jobs"
    if jobs.is_symlink() or (jobs.exists() and not jobs.is_dir()):
        raise JobError("unsafe_metadata_path", "The installation job metadata path is unsafe.")
    jobs.mkdir(exist_ok=True)
    logs = meta / "logs"
    if logs.is_symlink() or (logs.exists() and not logs.is_dir()):
        raise JobError("unsafe_log_path", "The installation log path is unsafe.")
    return root, meta, jobs


def owned_tree_identity(root, cancel=None):
    root = Path(root).resolve(strict=True)
    digest = hashlib.sha256(b"recomp-owned-tree-v1\n")
    stack = [(root, 0)]
    entries = 0
    total = 0
    while stack:
        directory, depth = stack.pop()
        for child in sorted(os.scandir(directory), key=lambda entry: unicodedata.normalize("NFC", entry.name)):
            if cancel:
                cancel()
            entries += 1
            if entries > MAX_PAYLOAD_FILES * 2:
                raise JobError("owned_tree_entry_limit", "The built app contains too many entries to fingerprint.")
            path = Path(child.path)
            relative = unicodedata.normalize("NFC", path.relative_to(root).as_posix())
            if len(relative.encode("utf-8")) > MAX_PATH_BYTES:
                raise JobError("owned_tree_path_limit", "A built app path is too long to fingerprint safely.")
            if child.is_symlink():
                target = os.readlink(path)
                resolved = (path.parent / target).resolve(strict=False)
                if not resolved.is_relative_to(root):
                    raise JobError("owned_tree_escape", "The built app contains a link outside its bundle.",
                                   details={"path": relative})
                payload = target.encode("utf-8")
                digest.update(b"L" + relative.encode("utf-8") + b"\0" + payload + b"\0")
            elif child.is_dir(follow_symlinks=False):
                if depth >= MAX_PATH_DEPTH:
                    raise JobError("owned_tree_depth_limit", "The built app is nested too deeply to fingerprint safely.")
                stack.append((path, depth + 1))
            elif child.is_file(follow_symlinks=False):
                info = path.stat()
                file_hash = hashlib.sha256()
                read = 0
                with path.open("rb") as stream:
                    while True:
                        if cancel:
                            cancel()
                        chunk = stream.read(min(HASH_CHUNK, MAX_PAYLOAD_BYTES - total + 1))
                        if not chunk:
                            break
                        read += len(chunk)
                        total += len(chunk)
                        if total > MAX_PAYLOAD_BYTES:
                            raise JobError("owned_tree_size_limit", "The built app exceeds its fingerprint size limit.")
                        file_hash.update(chunk)
                if read != info.st_size:
                    raise JobError("owned_tree_changed", "A built app file changed while it was fingerprinted.",
                                   details={"path": relative})
                digest.update(b"F" + relative.encode("utf-8") + b"\0" + str(info.st_size).encode() + b"\0")
                digest.update(file_hash.digest())
            else:
                raise JobError("owned_tree_special_file", "The built app contains a special file.",
                               details={"path": relative})
    return {"algorithm": "sha256", "sha256": digest.hexdigest(), "files": entries, "bytes": total}


def directory_token(path):
    info = Path(path).stat(follow_symlinks=False)
    if not stat.S_ISDIR(info.st_mode):
        raise JobError("unsafe_rename_target", "A publication path is not a regular directory.")
    return {"device": info.st_dev, "inode": info.st_ino}


def matches_directory_token(path, token):
    try:
        return directory_token(path) == token
    except OSError:
        return False


def rotate_logs(log_dir, current=None, keep_count=MAX_RETAINED_LOGS,
                keep_bytes=MAX_RETAINED_LOG_BYTES):
    log_dir = Path(log_dir)
    if log_dir.is_symlink():
        raise JobError("unsafe_log_path", "The job log folder is a symbolic link.")
    log_dir.mkdir(parents=True, exist_ok=True)
    current = Path(current) if current is not None else None
    if current is not None:
        if current.parent.resolve() != log_dir.resolve() or current.is_symlink():
            raise JobError("unsafe_log_path", "The current attempt log is outside its job log folder.")
        if not current.exists():
            fd = os.open(current, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            os.close(fd)
        if not current.is_file():
            raise JobError("unsafe_log_path", "The current attempt log is not a regular file.")
    logs = []
    for path in log_dir.iterdir():
        if path.is_symlink() or not path.is_file() or path.suffix != ".log":
            continue
        size = path.stat().st_size
        logs.append((path.stat().st_mtime_ns, path, size))
        if len(logs) > 4096:
            raise JobError("log_count_limit", "The job log folder contains too many log files to rotate safely.")
    logs.sort(key=lambda item: (item[0], item[1].name), reverse=True)
    retained = []
    if current is not None:
        size = current.stat().st_size
        if size > MAX_ATTEMPT_LOG_BYTES or size > keep_bytes:
            raise JobError("attempt_log_size_limit", "The current attempt log exceeds its retention limit.")
        retained.append(current)
        retained_bytes = MAX_ATTEMPT_LOG_BYTES
    else:
        retained_bytes = 0
    for _, path, size in logs:
        if path == current:
            continue
        if len(retained) >= keep_count or retained_bytes + size > keep_bytes:
            path.unlink()
        else:
            retained.append(path)
            retained_bytes += size


def copy_payload(source, destination, cancel=None, expected_bytes=None, exclude_paths=None, progress=None):
    source = Path(source).resolve(strict=True)
    destination = Path(destination)
    if destination.exists() or destination.is_symlink() or destination.resolve().is_relative_to(source):
        raise JobError("unsafe_copy_destination", "The staged copy path overlaps the source or already exists.")
    stack = [(source, destination, 0)]
    entries = 0
    total = 0
    file_count = 0
    excluded = {unicodedata.normalize("NFC", Path(value).as_posix()).casefold()
                for value in (exclude_paths or [])}
    if expected_bytes is not None and shutil.disk_usage(destination.parent).free < expected_bytes:
        raise JobError("insufficient_space", "There may not be enough free space to stage this game copy.")
    while stack:
        source_dir, destination_dir, depth = stack.pop()
        destination_dir.mkdir(parents=True, exist_ok=True)
        for child in os.scandir(source_dir):
            if cancel:
                cancel()
            entries += 1
            if entries > MAX_PAYLOAD_FILES * 2:
                raise JobError("payload_entry_limit", "The game copy contains too many entries to stage safely.")
            src = Path(child.path)
            dst = destination_dir / child.name
            relative = src.relative_to(source).as_posix()
            folded = unicodedata.normalize("NFC", relative).casefold()
            if any(folded == value or folded.startswith(value + "/") for value in excluded):
                continue
            if len(relative.encode("utf-8")) > MAX_PATH_BYTES:
                raise JobError("payload_path_limit", "A game path is too long to stage safely.", details={"path": str(src)})
            if child.is_symlink():
                raise JobError("source_symlink", "The selected game copy contains a symbolic link.", details={"path": str(src)})
            if child.is_dir(follow_symlinks=False):
                if depth >= MAX_PATH_DEPTH:
                    raise JobError("payload_depth_limit", "The game folder is nested too deeply to stage safely.")
                stack.append((src, dst, depth + 1))
            elif child.is_file(follow_symlinks=False):
                if dst.exists() or dst.is_symlink():
                    raise JobError("copy_path_collision", "Two source paths collide in the staged game copy.")
                before = src.stat()
                if before.st_size > MAX_PAYLOAD_BYTES or total + before.st_size > MAX_PAYLOAD_BYTES:
                    raise JobError("payload_size_limit", "The staged game copy exceeds its size limit.")
                file_bytes = 0
                with src.open("rb") as input_stream, dst.open("xb") as output_stream:
                    opened = os.fstat(input_stream.fileno())
                    if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                        raise JobError("source_changed", "A game file changed while it was being copied.", details={"path": str(src)})
                    while True:
                        if cancel:
                            cancel()
                        remaining = MAX_PAYLOAD_BYTES - total - file_bytes
                        chunk = input_stream.read(min(HASH_CHUNK, remaining + 1))
                        if not chunk:
                            break
                        file_bytes += len(chunk)
                        if total + file_bytes > MAX_PAYLOAD_BYTES:
                            raise JobError("payload_size_limit", "The staged game copy exceeds its size limit.")
                        output_stream.write(chunk)
                        if progress:
                            progress(total + file_bytes, relative)
                    output_stream.flush()
                    os.fsync(output_stream.fileno())
                after = src.stat()
                if file_bytes != before.st_size or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
                        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
                    raise JobError("source_changed", "A game file changed while it was being copied.", details={"path": str(src)})
                total += file_bytes
                file_count += 1
                shutil.copystat(src, dst, follow_symlinks=False)
            else:
                raise JobError("source_special_file", "The selected game copy contains a special file.", details={"path": str(src)})
    return {"files": file_count, "bytes": total}


def payload_files(root, cancel=None, exclude_paths=None):
    root = Path(root).resolve(strict=True)
    files = []
    entries = 0
    excluded = {unicodedata.normalize("NFC", Path(value).as_posix()).casefold()
                for value in (exclude_paths or [])}
    stack = [(root, 0)]
    while stack:
        directory, depth = stack.pop()
        try:
            with os.scandir(directory) as children:
                for child in children:
                    if cancel:
                        cancel()
                    entries += 1
                    if entries > MAX_PAYLOAD_FILES * 2:
                        raise JobError("payload_entry_limit", "The game copy contains too many entries to fingerprint safely.")
                    path = Path(child.path)
                    relative = unicodedata.normalize("NFC", path.relative_to(root).as_posix())
                    folded = relative.casefold()
                    if any(folded == value or folded.startswith(value + "/") for value in excluded):
                        continue
                    if child.is_symlink():
                        raise JobError("source_symlink", "The selected game copy contains a symbolic link.",
                                       ["Choose a source without symbolic links."], {"path": str(path)})
                    if child.is_dir(follow_symlinks=False):
                        relative = path.relative_to(root).as_posix()
                        if depth >= MAX_PATH_DEPTH:
                            raise JobError("payload_depth_limit", "The game folder is nested too deeply to fingerprint safely.")
                        if len(relative.encode("utf-8")) > MAX_PATH_BYTES:
                            raise JobError("payload_path_limit", "A game path is too long to fingerprint safely.", details={"path": relative})
                        stack.append((path, depth + 1))
                    elif child.is_file(follow_symlinks=False):
                        relative = path.relative_to(root).as_posix()
                        if len(relative.encode("utf-8")) > MAX_PATH_BYTES:
                            raise JobError("payload_path_limit", "A game path is too long to fingerprint safely.", details={"path": relative})
                        files.append(path)
                    else:
                        raise JobError("source_special_file", "The selected game copy contains a special file.",
                                       ["Choose a source containing regular files and folders only."], {"path": str(path)})
                    if len(files) > MAX_PAYLOAD_FILES:
                        raise JobError("payload_file_limit", "The game copy contains too many files to fingerprint safely.")
        except OSError as error:
            raise JobError("source_unreadable", "A game folder could not be fingerprinted.",
                           ["Check the source folder permissions."], {"path": str(directory), "error": str(error)}) from error
    normalized = {}
    for path in files:
        relative = unicodedata.normalize("NFC", path.relative_to(root).as_posix())
        key = relative.casefold()
        if key in normalized:
            raise JobError("payload_path_collision", "The game copy contains paths that collide by case or Unicode spelling.",
                           details={"paths": [normalized[key], relative]})
        normalized[key] = relative
    return root, sorted(files, key=lambda item: unicodedata.normalize("NFC", item.relative_to(root).as_posix()))


def payload_identity(root, cancel=None, exclude_paths=None, progress=None):
    root, files = payload_files(root, cancel, exclude_paths)
    digest = hashlib.sha256(b"recomp-payload-v1\n")
    total = 0
    for path in files:
        relative = unicodedata.normalize("NFC", path.relative_to(root).as_posix())
        info = path.stat()
        if info.st_size > MAX_PAYLOAD_BYTES or total + info.st_size > MAX_PAYLOAD_BYTES:
            raise JobError("payload_size_limit", "The game copy exceeds the payload fingerprint size limit.")
        file_hash = hashlib.sha256()
        read = 0
        with path.open("rb") as stream:
            opened = os.fstat(stream.fileno())
            initial = (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)
            if (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns) != initial:
                raise JobError("source_changed", "A game file changed during fingerprinting.",
                               ["Inspect the source again and retry."], {"path": relative})
            while True:
                if cancel:
                    cancel()
                remaining = MAX_PAYLOAD_BYTES - total - read
                chunk = stream.read(min(HASH_CHUNK, remaining + 1))
                if not chunk:
                    break
                read += len(chunk)
                if total + read > MAX_PAYLOAD_BYTES:
                    raise JobError("payload_size_limit", "The game copy exceeds the payload fingerprint size limit.")
                file_hash.update(chunk)
                if progress:
                    progress(total + read, relative)
        after = path.stat()
        if initial != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
            raise JobError("source_changed", "A game file changed during fingerprinting.",
                           ["Inspect the source again and retry."], {"path": relative})
        if read != info.st_size:
            raise JobError("source_changed", "A game file changed during fingerprinting.",
                           ["Inspect the source again and retry."], {"path": relative})
        total += read
        digest.update(relative.encode("utf-8") + b"\0" + str(info.st_size).encode() + b"\0")
        digest.update(file_hash.digest())
    return {"algorithm": "sha256", "sha256": digest.hexdigest(), "files": len(files), "bytes": total}


def payload_size(root, cancel=None, exclude_paths=None):
    root, files = payload_files(root, cancel, exclude_paths)
    total = 0
    for path in files:
        if cancel:
            cancel()
        total += path.stat().st_size
        if total > MAX_PAYLOAD_BYTES:
            raise JobError("payload_size_limit", "The game copy exceeds the payload size limit.")
    return {"files": len(files), "bytes": total}


def recipe_identity(kit, game_dir):
    kit = Path(kit).resolve()
    game_dir = Path(game_dir).resolve()
    files = set()
    for name in RECIPE_FILES:
        path = kit / name
        if path.is_file():
            if path.is_symlink():
                raise JobError("recipe_symlink", "The build recipe contains a symbolic link.", details={"path": str(path)})
            files.add(path)
    entries = 0
    for relative in RECIPE_DIRS:
        directory = kit / relative
        if directory.is_dir():
            stack = [directory]
            while stack:
                current = stack.pop()
                for child in os.scandir(current):
                    entries += 1
                    if entries > MAX_RECIPE_FILES:
                        raise JobError("recipe_entry_limit", "The build recipe contains too many input entries.")
                    path = Path(child.path)
                    relative_path = path.relative_to(directory)
                    if any(part in IGNORED_PARTS for part in relative_path.parts):
                        continue
                    if child.is_symlink():
                        raise JobError("recipe_symlink", "The build recipe contains a symbolic link.",
                                       details={"path": str(path)})
                    if child.is_dir(follow_symlinks=False):
                        stack.append(path)
                    elif child.is_file(follow_symlinks=False):
                        files.add(path)
                    else:
                        raise JobError("recipe_special_file", "The build recipe contains a special file.",
                                       details={"path": str(path)})
    tools = kit / "tools/recomp"
    if tools.is_dir():
        stack = [tools]
        while stack:
            current = stack.pop()
            with os.scandir(current) as children:
                for child in children:
                    entries += 1
                    if entries > MAX_RECIPE_FILES:
                        raise JobError("recipe_entry_limit", "The build recipe contains too many input entries.")
                    path = Path(child.path)
                    relative = path.relative_to(tools)
                    if any(part in IGNORED_PARTS for part in relative.parts):
                        continue
                    if child.is_symlink():
                        raise JobError("recipe_symlink", "The build recipe contains a symbolic link.",
                                       details={"path": str(path)})
                    if child.is_dir(follow_symlinks=False):
                        stack.append(path)
                    elif child.is_file(follow_symlinks=False) and path.suffix == ".py":
                        files.add(path)
    for child in os.scandir(kit / "tools"):
        path = Path(child.path)
        if path.suffix == ".py" and path.name not in RECIPE_TOOL_EXCLUDES:
            if child.is_symlink():
                raise JobError("recipe_symlink", "The build recipe contains a symbolic link.",
                               details={"path": str(path)})
            if child.is_file(follow_symlinks=False):
                files.add(path)
    if game_dir.is_dir():
        stack = [game_dir]
        while stack:
            current = stack.pop()
            for child in os.scandir(current):
                entries += 1
                if entries > MAX_RECIPE_FILES:
                    raise JobError("recipe_entry_limit", "The build recipe contains too many input entries.")
                path = Path(child.path)
                relative = path.relative_to(game_dir)
                if path.suffix.casefold() == ".md" or any(part in IGNORED_PARTS for part in relative.parts):
                    continue
                if child.is_symlink():
                    raise JobError("recipe_symlink", "The build recipe contains a symbolic link.",
                                   details={"path": str(path)})
                if child.is_dir(follow_symlinks=False):
                    stack.append(path)
                elif child.is_file(follow_symlinks=False):
                    files.add(path)
                else:
                    raise JobError("recipe_special_file", "The build recipe contains a special file.",
                                   details={"path": str(path)})
    digest = hashlib.sha256(b"recomp-recipe-v1\n")
    dependencies = kit.parent / "dependencies/manifest.json"
    if dependencies.is_file():
        digest.update(b"bundled-dependencies\0" + dependencies.read_bytes())
    total = 0
    for path in sorted(files, key=lambda item: unicodedata.normalize("NFC", item.relative_to(kit).as_posix())):
        digest.update(path.relative_to(kit).as_posix().encode("utf-8") + b"\0")
        file_hash = hashlib.sha256()
        before = path.stat()
        read = 0
        with path.open("rb") as stream:
            opened = os.fstat(stream.fileno())
            if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                raise JobError("recipe_changed", "A build input changed while its recipe was fingerprinted.",
                               details={"path": str(path)})
            while True:
                chunk = stream.read(HASH_CHUNK)
                if not chunk:
                    break
                read += len(chunk)
                total += len(chunk)
                if total > MAX_RECIPE_BYTES:
                    raise JobError("recipe_size_limit", "The build recipe exceeds its fingerprint size limit.")
                file_hash.update(chunk)
        after = path.stat()
        if read != before.st_size or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
                after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
            raise JobError("recipe_changed", "A build input changed while its recipe was fingerprinted.",
                           details={"path": str(path)})
        digest.update(file_hash.digest())
    toolchain = {}
    for command in ("cmake", "ninja", "clang", "clang++", "cargo", "rustc"):
        executable = shutil.which(command)
        if executable:
            try:
                result = subprocess.run([executable, "--version"], capture_output=True, text=True,
                                        timeout=10, check=False)
                toolchain[command] = {"path": executable, "version": (result.stdout or result.stderr).splitlines()[0]
                                      if result.stdout or result.stderr else str(result.returncode)}
            except (OSError, subprocess.TimeoutExpired):
                toolchain[command] = {"path": executable, "version": "unavailable"}
    options = {key: os.environ.get(key, "") for key in
               ("CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS", "SDKROOT",
                "MACOSX_DEPLOYMENT_TARGET", "CMAKE_PREFIX_PATH", "RUSTFLAGS", "CMAKE_ARGS", "MAKEFLAGS")}
    options_hash = hashlib.sha256(json.dumps(options, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    digest.update(json.dumps({"platform": platform.platform(), "toolchain": toolchain,
                             "optionsSha256": options_hash}, sort_keys=True,
                            separators=(",", ":")).encode())
    return {"algorithm": "sha256", "sha256": digest.hexdigest(), "toolchain": toolchain,
            "platform": platform.platform(), "optionsSha256": options_hash}
