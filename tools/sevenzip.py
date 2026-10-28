import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


VERSION = "26.05"
URL = "https://github.com/ip7z/7zip/releases/download/26.05/7z2605-mac.tar.xz"
SHA256 = "12ef6830317743cfd3f0ab1724bde43fcc270d8b6d8564f8c12c2a2b7f993570"
SOURCE_URL = "https://github.com/ip7z/7zip/releases/download/26.05/7z2605-src.tar.xz"
SOURCE_SHA256 = "0b6080b8c3e7a579161d2d495224c0e51c467576cb4c7048b0ab485ef44cc22e"


class DependencyError(ValueError):
    def __init__(self, label, reason):
        super().__init__(f"Could not obtain {label}: {reason}")
        self.label = label
        self.reason = reason


class HttpsRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not newurl.startswith("https://"):
            raise ValueError("Dependency download redirected outside HTTPS")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def matches(path, digest, size=None):
    if path.is_symlink() or not path.is_file() or (size is not None and path.stat().st_size != size):
        return False
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest() == digest


def download(url, destination, digest, size, cancel=lambda: None):
    destination = Path(destination)
    if matches(destination, digest, size):
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
            temporary = Path(output.name)
            with urllib.request.build_opener(HttpsRedirects).open(url, timeout=30) as response:
                total = 0
                while True:
                    cancel()
                    block = response.read(64 * 1024)
                    if not block:
                        break
                    total += len(block)
                    if total > size:
                        raise ValueError("Dependency download exceeds its declared size")
                    output.write(block)
        if not matches(temporary, digest, size):
            raise ValueError("Dependency download does not match its pinned size and SHA-256")
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def provision(build_root):
    if sys.platform != "darwin" or platform.machine() not in ("arm64", "x86_64"):
        raise ValueError("The pinned prebuilt 7zz currently supports macOS arm64 and x86_64")
    root = Path(build_root) / "toolkit/extractors/sevenzip"
    archive = root / "7z2605-mac.tar.xz"
    download(URL, archive, SHA256, 1868068)
    download(SOURCE_URL, root / "7z2605-src.tar.xz", SOURCE_SHA256, 1565688)
    binary = root / "7zz"
    stamp = root / "7zz.stamp"
    if stamp.is_file() and (root / "License.txt").is_file() and os.access(binary, os.X_OK):
        recorded = stamp.read_text().split()
        if len(recorded) == 3 and recorded[:2] == [SHA256, platform.machine()] and matches(binary, recorded[2]):
            return root
    stamp.unlink(missing_ok=True)
    with tarfile.open(archive, "r:xz") as package:
        for name in ("7zz", "License.txt"):
            member = package.getmember(name)
            if not member.isfile() or member.size > 16 * 1024 * 1024:
                raise ValueError("Unexpected member in the pinned 7zz distribution")
            with package.extractfile(member) as source, (root / name).open("wb") as target:
                shutil.copyfileobj(source, target)
    thin = root / "7zz-thin"
    subprocess.run(["lipo", str(binary), "-thin", platform.machine(), "-output", str(thin)], check=True)
    thin.replace(binary)
    binary.chmod(0o755)
    stamp.write_text(f"{SHA256} {platform.machine()} {hashlib.sha256(binary.read_bytes()).hexdigest()}\n")
    return root


def executable():
    name = "7zz.exe" if os.name == "nt" else "7zz"
    candidates = [Path(sys.executable).parent / name]
    if os.environ.get("RECOMP_7ZZ"):
        candidates.append(Path(os.environ["RECOMP_7ZZ"]))
    found = next((path for path in candidates if path.is_file() and os.access(path, os.X_OK)), None)
    if found:
        return found
    local = shutil.which("7zz") or shutil.which("7z")
    if local:
        return Path(local)
    raise ValueError("7-Zip is unavailable. Install 7zz or 7z locally, or reinstall the packaged toolkit.")


def extract_member(archive, member, destination, size, cancel=lambda: None):
    with tempfile.TemporaryFile() as errors, destination.open("wb") as output:
        process = subprocess.Popen([str(executable()), "e", "-so", "-spd", "-y", "-bd", "-bb0",
                                    str(archive), member], stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=errors)
        try:
            while True:
                cancel()
                if output.tell() > size or errors.tell() > 64 * 1024:
                    raise ValueError("7zz extraction exceeds its declared output limits")
                try:
                    process.wait(timeout=0.05)
                    break
                except subprocess.TimeoutExpired:
                    pass
            if process.returncode or output.tell() != size:
                errors.seek(0)
                detail = errors.read(8192).decode("utf-8", errors="replace")
                raise ValueError(f"7zz could not extract {member}: {detail or process.returncode}")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()


def acquire_dependencies(cfg, executable_path, workspace, cancel=lambda: None, progress=lambda message: None):
    changed = False
    for item in cfg["setup"].get("downloads", []):
        cancel()
        destination = executable_path.parent / item["path"]
        if os.path.lexists(destination):
            continue
        try:
            progress("Downloading " + item["label"])
            archive = workspace / item["archive_sha256"]
            download(item["url"], archive, item["archive_sha256"], item["archive_size"], cancel)
            with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
                temporary = Path(output.name)
            try:
                progress("Extracting " + item["label"])
                extract_member(archive, item["member"], temporary, item["size"], cancel)
                if not matches(temporary, item["sha256"], item["size"]):
                    raise ValueError("the extracted file does not match its pinned SHA-256")
                temporary.chmod(0o644)
                temporary.replace(destination)
                changed = True
            finally:
                temporary.unlink(missing_ok=True)
        except InterruptedError:
            raise
        except (OSError, ValueError) as error:
            raise DependencyError(item["label"], str(error)) from error
    return changed
