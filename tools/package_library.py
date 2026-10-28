import json
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib
from sign_macos import sign_bundle
from build import rust_translator

ROOT = Path(__file__).resolve().parents[1]


def bundle_extractors(resources, build_root):
    from sevenzip import provision, VERSION, URL, SHA256, SOURCE_URL, SOURCE_SHA256
    provision(build_root)
    build = Path(build_root) / "toolkit/extractors"
    tools = [(build / "install/bin/innoextract", resources / "python/bin/innoextract"),
             (build / "sevenzip/7zz", resources / "python/bin/7zz"),
             (build / "unshield/lib/libunshield.dylib", resources / "python/bin/libunshield.dylib")]
    for source, target in tools:
        if not source.is_file():
            raise ValueError(f"Missing source-built extractor: {source}. Build with --target extractors first.")
        shutil.copy2(source, target)
        subprocess.run(["lipo", str(target), "-verify_arch", "arm64"], check=True)
        commands = subprocess.check_output(["otool", "-l", str(target)], text=True)
        versions = re.findall(r"\bminos\s+(\d+)\.(\d+)", commands)
        versions += re.findall(r"cmd LC_VERSION_MIN_MACOSX\s+cmdsize \d+\s+version (\d+)\.(\d+)", commands)
        if any(tuple(map(int, version)) > (14, 0) for version in versions):
            raise ValueError(f"Extractor requires newer than macOS 14: {source}")
        linked = subprocess.check_output(["otool", "-L", str(target)], text=True).splitlines()
        for row in linked[2 if target.suffix == ".dylib" else 1:]:
            dependency = row.strip().split(" (compatibility version", 1)[0]
            if not dependency.startswith(("/usr/lib/", "/System/Library/")):
                raise ValueError(f"Extractor has an unbundled dependency: {dependency} in {source}")
        if target.suffix == ".dylib":
            subprocess.run(["install_name_tool", "-id", "@loader_path/" + target.name, str(target)], check=True)
    sources = build / "sources"
    for name, files in (
            ("sevenzip", [build / "sevenzip/License.txt", build / "sevenzip/7z2605-src.tar.xz"]),
            ("innoextract", [sources / "innoextract/LICENSE"]),
            ("boost", [sources / "boost/LICENSE_1_0.txt"]),
            ("xz", [sources / "xz/COPYING", sources / "xz/COPYING.0BSD"]),
            ("unshield", [ROOT / "third_party/unshield/LICENSE", ROOT / "third_party/unshield/lib/md5/md5c.c"])):
        destination = resources / "licenses/extractors" / name
        destination.mkdir(parents=True, exist_ok=True)
        for source in files:
            shutil.copy2(source, destination / source.name)
    return {"macosMinimum": "14.0", "innoextract": "1.9", "boost": "1.82.0", "xz": "5.8.1",
            "sevenzip": {"version": VERSION, "url": URL, "sha256": SHA256,
                         "sourceUrl": SOURCE_URL, "sourceSha256": SOURCE_SHA256},
            "unshield": {"version": "1.6.2", "source": "https://github.com/twogood/unshield/tree/1.6.2",
                         "sourceSha256": "a937ef596ad94d16e7ed2c8553ad7be305798dcdcfd65ae60210b1e54ab51a2f"},
            "inputs": {target.relative_to(resources).as_posix(): hashlib.sha256(source.read_bytes()).hexdigest()
                       for source, target in tools}}


def package(bundle, build_root, python_root):
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        raise ValueError("Recomp Toolkit packaging currently requires native arm64 macOS")
    python_root = Path(python_root).resolve()
    if not (python_root / "bin/python3").is_file():
        raise ValueError("Provide a standalone Python installation with --toolkit-python-root")
    dependency_build = Path(build_root) / "toolkit-dependencies"
    subprocess.run([sys.executable, str(ROOT / "tools/build.py"), "--game", "stub", "--stub",
                    "--target", "app", "--build-root", str(dependency_build)], check=True,
                   env=dict(os.environ, MACOSX_DEPLOYMENT_TARGET="14.0"))
    translator = rust_translator(dependency_build)
    resources = bundle / "Contents/Resources"
    if resources.exists():
        for child in resources.iterdir():
            if child.name != "icon.icns":
                if child.is_dir() and not child.is_symlink():
                    shutil.rmtree(child)
                else:
                    child.unlink()
    resources.mkdir(parents=True, exist_ok=True)
    kit = resources / "kit"
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0")
    for name in filter(None, tracked):
        source = ROOT / name
        if source.is_symlink():
            raise ValueError(f"Unexpected source symlink: {name}")
        destination = kit / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    python = resources / "python"
    shutil.copytree(python_root, python, symlinks=True,
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    subprocess.run([str(python / "bin/python3"), "-m", "pip", "install", "--only-binary=:all:",
                    "cmake==4.1.2", "ninja==1.13.0"], check=True)
    subprocess.run([str(python / "bin/python3"), "-m", "pip", "install", "--only-binary=:all:",
                    "-r", str(ROOT / "tools/requirements-library.txt")], check=True)
    packages = next((python / "lib").glob("python*/site-packages"))
    for name, source in (("cmake", packages / "cmake/data/bin/cmake"),
                         ("ninja", packages / "ninja/data/bin/ninja")):
        if name == "ninja" and not source.exists():
            source = python / "bin/ninja"
        destination = python / "bin" / name
        if source != destination:
            destination.unlink(missing_ok=True)
            shutil.copy2(source, destination)
        thin = destination.with_suffix(".arm64")
        subprocess.run(["lipo", str(destination), "-thin", "arm64", "-output", str(thin)], check=True)
        thin.replace(destination)
    cmake_data = packages / "cmake/data"
    (python / "share").mkdir(exist_ok=True)
    for path in (cmake_data / "share").glob("cmake-*"):
        shutil.move(str(path), python / "share" / path.name)
    shutil.rmtree(cmake_data)
    for name in ("ctest", "cpack"):
        (python / "bin" / name).unlink(missing_ok=True)
    dependencies = resources / "dependencies"
    dependencies.mkdir()
    (dependencies / "bin").mkdir()
    shutil.copy2(translator, dependencies / "bin/recomp-translate")
    cmake_build = dependency_build / "cmake/macos-stub"
    subprocess.run([str(python / "bin/cmake"), "--install", str(cmake_build / "_deps/sdl3-build"),
                    "--prefix", str(dependencies / "sdl")], check=True)
    if not (dependencies / "sdl/lib/cmake/SDL3/SDL3Config.cmake").is_file():
        raise ValueError("SDL did not install its development package")
    shutil.copy2(cmake_build / "_deps/sdl3-src/LICENSE.txt", dependencies / "sdl/LICENSE.txt")
    shutil.copytree(cmake_build / "ffmpeg", dependencies / "ffmpeg", symlinks=True)
    shutil.copy2(cmake_build / "lib/liblua.a", dependencies / "liblua.a")
    shutil.copy2(ROOT / "third_party/ffmpeg/NOTICE.md", dependencies / "ffmpeg/NOTICE.md")
    sources = resources / "sources"
    sources.mkdir()
    shutil.copy2(cmake_build / "ffmpeg-prefix/src/ffmpeg-7.1.1.tar.xz", sources)
    registry = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "registry/src"
    for lock in (ROOT / "desktop/library/Cargo.lock", ROOT / "graphics/d3d8-wgpu/Cargo.lock",
                 ROOT / "translator/Cargo.lock"):
        for entry in tomllib.loads(lock.read_text())["package"]:
            name = f"{entry['name']}-{entry['version']}"
            for crate in registry.glob(f"*/{name}"):
                for path in crate.rglob("*"):
                    if path.is_file() and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE", "COPYRIGHT")):
                        destination = resources / "licenses/cargo" / name / path.relative_to(crate)
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(path, destination)
    bridge = dependencies / "d3d8"
    bridge.mkdir()
    for name in ("d3d_bridge.h", "native-libs.rsp"):
        shutil.copy2(dependency_build / "d3d8-generated" / name, bridge / name)
    shutil.copytree(dependency_build / "d3d8-generated/rust", bridge / "rust")
    shutil.copy2(dependency_build / "d3d8-wgpu/rust/release/libd3d8_wgpu.a", bridge)
    source_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    manifest = {"source": source_sha, "architecture": "arm64", "macosMinimum": "14.0"}
    (dependencies / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for path in python.rglob("__pycache__"):
        shutil.rmtree(path)
    extractors = bundle_extractors(resources, build_root)
    (resources / "extractors.json").write_text(json.dumps(extractors, indent=2) + "\n")
    sign_bundle(bundle)
    archive = bundle.parent / "recomp-toolkit-macos-arm64.zip"
    subprocess.run(["ditto", "-c", "-k", "--keepParent", str(bundle), str(archive)], check=True)
    print(f"Portable Recomp Toolkit: {archive} ({archive.stat().st_size / 1048576:.1f} MiB)")
