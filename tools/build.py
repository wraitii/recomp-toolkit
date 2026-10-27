#!/usr/bin/env python3
"""Build the native app through CMake, regenerating original-game code only when needed.

The game is a directory holding game.toml (tools/build.py --game-dir). Its
outputs (the translation, the texture pack, the apps, the logs) go under
<game-dir>/build when the game lives outside the kit, else under the kit's
build/. The kit's own default is games/stub, a game that does not exist."""

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import hashlib
import sys
import tempfile
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/recomp"))
sys.path.insert(0, str(ROOT / "tools"))
import game_config  # noqa: E402
import buildlock  # noqa: E402
import import_abi  # noqa: E402
import copy_layouts  # noqa: E402
import package_desktop  # noqa: E402
import stage_game_files  # noqa: E402
import web_launcher  # noqa: E402

# What each --target builds. `plugins` is every mod plugin the game ships.
TARGETS = {
    "toolkit": [],
    "extractors": [],
    "app": ["recomp_app"],
    "smoke": ["recomp_smoke"],
    "headless": ["recomp_headless"],
    "gen": ["recomp_gen"],
    "dispatch-tests": ["dispatch_tests"],
    "interp-tests": ["interp_tests"],
    "plugins": ["plugins"],
    "ios": ["recomp_app"],
    "android": ["recomp_app"],
    "web": ["recomp_app"],
}
MACOS_ONLY = {"ios"}
NEEDS_GEN = {"app", "smoke", "headless", "gen", "dispatch-tests", "interp-tests", "ios", "android", "web"}
# Targets with presets of their own, whatever the host system.
OWN_PRESET = {"ios", "android", "web"}


def default_preset(system=None):
    """The CMake preset for this operating system."""
    return {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}[system or platform.system()]


def preset_name(preset, config, stub=False, target=None):
    """Debug, stub, mobile and web builds use separate presets and binary directories."""
    if target in OWN_PRESET:
        return target + "-stub" if stub else target
    if stub:
        return preset + "-stub"
    return preset if config == "Release" else preset + "-debug"


build_root_for = game_config.build_root_for


def build_dir_for(build_root, preset):
    """The CMake binary directory: what the presets name inside the kit, beside the game outside it."""
    return Path(build_root) / "cmake" / preset


def archive_path(build_root, system=None, preset=None):
    """Where CMake writes the translated archive on this platform."""
    name = "recomp_gen.lib" if (system or platform.system()) == "Windows" else "librecomp_gen.a"
    return Path(build_root) / "cmake" / (preset or default_preset(system)) / "lib" / name


def cmake_tool(name):
    """Prefer the venv's pinned cmake/ctest beside this interpreter, then PATH."""
    beside = Path(sys.executable).parent / name
    if beside.exists():
        return str(beside)
    return shutil.which(name) or name


def game_defines(game_dir, build_root):
    """The cache paths and the game's [render] switches, passed on every configure."""
    render = game_config.load(game_dir, build_root)["render"]
    return ["-DRECOMP_GAME_DIR=%s" % Path(game_dir).as_posix(), "-DRECOMP_BUILD_ROOT=%s" % Path(build_root).as_posix(),
            "-DRECOMP_D3D8_WGPU=" + ("ON" if render["d3d8_wgpu"] else "OFF")]


def configure(preset, extra=(), build_dir=None):
    """Configure a preset; `build_dir` overrides the preset's binary directory."""
    command = [cmake_tool("cmake"), "--preset", preset, "-DPython3_EXECUTABLE=" + sys.executable]
    dependencies = ROOT.parent / "dependencies"
    if (dependencies / "manifest.json").is_file():
        manifest = json.loads((dependencies / "manifest.json").read_text())
        if platform.system() != "Darwin" or platform.machine() != manifest["architecture"]:
            raise ValueError("Bundled dependencies do not match this operating system and architecture")
        command += ["-DRECOMP_DEPENDENCIES=" + str(dependencies),
                    "-DCMAKE_MAKE_PROGRAM=" + str(Path(sys.executable).parent / "ninja"),
                    "-DCMAKE_OSX_DEPLOYMENT_TARGET=" + manifest["macosMinimum"]]
    if build_dir is not None:
        command += ["-B", str(build_dir)]
    subprocess.run(command + list(extra), cwd=ROOT, check=True)


def build(preset, targets, jobs, extra=(), build_dir=None, config="Release"):
    """`extra` goes after the targets: a leading "--" hands the rest to the native tool.

    A build directory named directly (not through the build preset) needs the
    configuration spelled out too: the Xcode generator is multi-config and
    would otherwise build Debug, whose -O0 translation overflows the guest's
    stack on the device."""
    command = [cmake_tool("cmake"), "--build"]
    command += [str(build_dir), "--config", config] if build_dir is not None else ["--preset", preset]
    command += ["--parallel", str(jobs), "--target"] + list(targets) + list(extra)
    subprocess.run(command, cwd=ROOT, check=True)


def devicectl_list():
    """Paired devices as devicectl reports them."""
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
        path = tmp.name
    subprocess.run(["xcrun", "devicectl", "list", "devices", "--json-output", path], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with open(path) as fh:
        return json.load(fh)["result"]["devices"]


def pick_device(devices):
    """The one paired iPad, or exit asking for --device."""
    ipads = [d for d in devices
             if d.get("hardwareProperties", {}).get("productType", "").startswith("iPad")
             and d.get("connectionProperties", {}).get("pairingState") == "paired"]
    if len(ipads) == 1:
        return ipads[0]["identifier"]
    names = ", ".join("%s (%s)" % (d.get("deviceProperties", {}).get("name", "?"), d["identifier"]) for d in ipads)
    sys.exit("Pass --device <identifier>; paired iPads: %s" % (names or "none"))


def ios_app_bundle(app_name, build_root):
    """The signed bundle under <build root>/ios; Xcode adds a configuration directory (Release-iphoneos)."""
    found = sorted((Path(build_root) / "ios").glob("**/%s.app" % app_name))
    if not found:
        sys.exit("No %s.app under %s/ios; did the iOS build succeed?" % (app_name, build_root))
    return found[-1]


def install_and_launch(app, bundle_id, device, console):
    """Install the bundle with devicectl and launch it, optionally streaming its console."""
    subprocess.run(["xcrun", "devicectl", "device", "install", "app", "--device", device, str(app)], check=True)
    launch = ["xcrun", "devicectl", "device", "process", "launch", "--terminate-existing", "--device", device]
    if console:
        launch.append("--console")
    launch.append(bundle_id)
    subprocess.run(launch, check=True)


def android_project(build_root, cfg, *, gen_dir):
    """Render the Gradle project without building it; gen_dir is the CMake binary directory.

    SDL's Java sources stay in that build's FetchContent checkout. Gradle
    only packages the native library built by CMake, never invokes CMake.
    """
    template = ROOT / "platform/android"
    out = Path(build_root) / "android"
    shutil.copytree(template, out, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns(".gradle", "build", ".gitignore", "local.properties"))
    values = {key: cfg["game"][key] for key in ("app_name", "bundle_id", "id")}
    values["sdl_java_dir"] = (Path(gen_dir).resolve() / "_deps/sdl3-src/android-project/app/src/main/java").as_posix()
    for original in template.rglob("*.in"):
        source = out / original.relative_to(template)
        text = source.read_text()
        for key, value in values.items():
            if source.name.endswith(".xml.in"):
                value = escape(value, {'"': "&quot;", "'": "&apos;"})
            else:
                value = json.dumps(value, ensure_ascii=False)[1:-1].replace("$", r"\$")
            text = text.replace("@%s@" % key, value)
        source.with_suffix("").write_text(text)
        source.unlink()
    return out


def android_stage_layout_assets(out, game_dir):
    """Put the game's control layouts in the APK's assets, as controls/.

    RecompActivity copies them out to the app's external files folder on
    start, which is where host_resource("controls") looks on Android. The
    directory is rebuilt from scratch so a layout the game dropped also
    leaves the APK, as the ffmpeg notice does when video goes off.
    """
    controls = Path(out) / "app/src/main/assets/controls"
    if controls.exists():
        shutil.rmtree(controls)
    return copy_layouts.copy_layouts(game_dir, controls)


def android_stage_core_assets(out, game_dir):
    """Package core manifests/data; native plugins are linked into libmain.so.

    Never carry a developer's desktop binaries, sources or debug bundles into
    the APK. Only the app-owned core tree is rebuilt; profiles are unrelated.
    """
    from copy_core_mods import copy_core_mods
    copy_core_mods(game_dir, Path(out) / "app/src/main/assets/mods/core")


def android_apk(build_root, cfg, *, gen_dir, game_dir):
    """Stage the native libraries beside the SDL activity and assemble a debug APK."""
    library = Path(gen_dir) / "host/libmain.so"
    sdl_activity = Path(gen_dir) / "_deps/sdl3-src/android-project/app/src/main/java/org/libsdl/app/SDLActivity.java"
    for path in (library, sdl_activity):
        if not path.is_file():
            raise FileNotFoundError("Android CMake build input is missing: %s" % path)
    out = android_project(build_root, cfg, gen_dir=gen_dir)
    jni = out / "app/src/main/jniLibs/arm64-v8a"
    jni.mkdir(parents=True, exist_ok=True)
    shutil.copy2(library, jni / "libmain.so")
    # Read the configured option, not leftover installed libraries: switching
    # video off must remove the previous build's copies from the APK too.
    cache = (Path(gen_dir) / "CMakeCache.txt").read_text().splitlines()
    video = any(line.startswith("RECOMP_VIDEO:BOOL=") and
                line.partition("=")[2].upper() in {"1", "ON", "YES", "TRUE", "Y"}
                for line in cache)
    for component in ("avformat", "avcodec", "avutil"):
        name = "lib%s.so" % component
        if video:
            shutil.copy2(Path(gen_dir) / "ffmpeg/lib" / name, jni / name)
        else:
            (jni / name).unlink(missing_ok=True)
    notice = out / "app/src/main/assets/ffmpeg-NOTICE.md"
    if video:
        notice.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / "third_party/ffmpeg/NOTICE.md", notice)
    else:
        notice.unlink(missing_ok=True)
    android_stage_layout_assets(out, game_dir)
    android_stage_core_assets(out, game_dir)
    wrapper = "gradlew.bat" if platform.system() == "Windows" else "./gradlew"
    subprocess.run([wrapper, "assembleDebug"], cwd=out, check=True)
    apk = out / "app/build/outputs/apk/debug/app-debug.apk"
    if not apk.is_file():
        raise FileNotFoundError("No APK after assembleDebug: %s" % apk)
    print("Packaged %s (%d bytes)" % (apk, apk.stat().st_size), flush=True)
    return apk


def android_push_game(command, cfg, build_root):
    """Stage the configured install, then push game/ without deleting device saves.

    Recreate only our generated staging directory so exclusions also apply to
    files staged by an earlier build. The shared stager uses excluded() and
    writes the executable hash to .stamp, just as it does for an iOS bundle.
    """
    source = cfg["developer_exe_path"].parent
    executable = cfg["game"]["executable"]
    if not (source / executable).is_file():
        raise FileNotFoundError("Prepare your own game installation with tools/setup.py before --push-game")
    staged = Path(build_root) / "android/game"
    if staged.exists():
        shutil.rmtree(staged)
    keep = [m["path"].relative_to(source).as_posix()
            for m in cfg["aux_modules"] if m["path"].is_relative_to(source)]
    count = stage_game_files.stage(source, staged, executable, cfg["bundle"]["exclude"], keep)

    destination = "/sdcard/Android/data/%s/files" % cfg["game"]["bundle_id"]
    print("Staged %d game files in %s; pushing to %s/game" % (count, staged, destination), flush=True)
    subprocess.run(command + ["shell", "mkdir", "-p", destination], check=True)
    # Push game/ into its parent on every run: never create game/game/.
    subprocess.run(command + ["push", str(staged), destination + "/"], check=True)


def android_install_and_launch(apk, bundle_id, device=None, console=False, *, game_cfg=None, build_root=None):
    """Install, optionally push game data, then launch on one ready adb device.

    An explicit data push requires a device; a build alone may skip device
    actions. The APK is installed first so Android owns the external files path.
    """
    adb = shutil.which("adb")
    if not adb and os.environ.get("ANDROID_HOME"):
        name = "adb.exe" if platform.system() == "Windows" else "adb"
        candidate = Path(os.environ["ANDROID_HOME"]) / "platform-tools" / name
        if candidate.is_file():
            adb = str(candidate)
    if not adb:
        if game_cfg is not None:
            raise ValueError("adb unavailable; --push-game requires Android platform-tools and a ready device")
        print("adb unavailable; skipped Android install, launch and logcat.")
        return
    result = subprocess.run([adb, "devices"], check=True, capture_output=True, text=True)
    devices = [fields[0] for line in result.stdout.splitlines()
               if len(fields := line.split()) == 2 and fields[1] == "device"]
    if not devices:
        if game_cfg is not None:
            raise ValueError("No Android device attached; --push-game requires a ready device in adb devices")
        print("No Android device attached; skipped install, launch and logcat.")
        return
    if device is not None and device not in devices:
        raise ValueError("Android device %s is not ready in adb devices" % device)
    if device is None and len(devices) != 1:
        raise ValueError("Pass --device <adb serial>; ready Android devices: %s" % ", ".join(devices))
    command = [adb, "-s", device or devices[0]]
    subprocess.run(command + ["install", "-r", str(apk)], check=True)
    if game_cfg is not None:
        android_push_game(command, game_cfg, build_root)
    subprocess.run(command + ["shell", "am", "start", "-n", bundle_id + "/dev.recompkit.RecompActivity"], check=True)
    if console:
        subprocess.run(command + ["logcat"], check=True)


def sync_file(src, dst):
    if dst.is_file() and dst.read_bytes() == src.read_bytes():
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    temporary = dst.with_name(dst.name + ".new")
    shutil.copy2(src, temporary)
    temporary.replace(dst)


def sync_tree(src, dst):
    """Make `dst` match `src`, rewriting only files whose bytes changed.

    Publishing a fresh tree by rename gives every generated source a new
    mtime, so ninja rebuilds every chunk even when the translation is byte
    for byte identical. Copying per file and leaving unchanged files alone
    keeps their mtimes, so a regeneration that touched one function only
    recompiles the chunks that function reached. Files `src` no longer
    carries are removed so a dropped chunk does not linger."""
    src_files = {p.relative_to(src): p for p in src.rglob("*") if p.is_file()}
    dst_files = {p.relative_to(dst): p for p in dst.rglob("*") if p.is_file()} if dst.is_dir() else {}
    for rel in sorted(set(dst_files) - set(src_files), reverse=True):
        dst_files[rel].unlink()
    for rel, sp in src_files.items():
        sync_file(sp, dst / rel)


def translation_input(cfg):
    """The resolved game configuration the Rust translator reads instead of game.toml."""
    def stable(value):
        if isinstance(value, Path):
            return str(value)
        if isinstance(value, bytes):
            return value.hex()
        if isinstance(value, dict):
            return {("%08x" % key if isinstance(key, int) else key): stable(item) for key, item in value.items()}
        if isinstance(value, (list, tuple, set, frozenset)):
            return [stable(item) for item in value]
        return value

    reloc = cfg.get("reloc")
    return {"format": "recomp-translate-input-v1", "dir": str(cfg["dir"]),
            "exe": str(cfg["developer_exe_path"]),
            "code_map": str(cfg["code_map_path"]) if cfg.get("code_map_path") else None,
            "image_base": cfg["game"]["image_base"], "sha256": cfg["game"]["sha256"],
            "store_hooks": cfg["game"]["store_hooks"], "call_hooks": cfg["game"]["call_hooks"],
            "translate": stable(cfg["translate"]), "globals": stable(cfg["globals"]),
            "curated": stable(cfg["curated"]), "aux_modules": stable(cfg["aux_modules"]),
            "reloc": str(reloc["manifest_path"]) if reloc else None,
            "import_pops": import_abi.import_pops()}


def rust_pcode_library(build_root):
    """Build the shared p-code evaluator library for this desktop host."""
    target = Path(build_root) / "recomp/cargo"
    subprocess.run(["cargo", "build", "--release", "--quiet", "--locked", "-p", "rk-pcode-ffi",
                    "--manifest-path", str(ROOT / "translator/Cargo.toml"), "--target-dir", str(target)],
                   check=True)
    names = {"Darwin": "librk_pcode_ffi.dylib", "Windows": "rk_pcode_ffi.dll"}
    name = names.get(platform.system(), "librk_pcode_ffi.so")
    source = target / "release" / name
    if platform.system() == "Darwin":
        staged = Path(build_root) / "recomp/pcode" / name
        staged.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, staged)
        subprocess.run(["install_name_tool", "-id", "@rpath/" + name, str(staged)], check=True)
        return staged
    return source


def rust_analysis(build_root):
    """Build the offline analysis producer and return its executable."""
    target = Path(build_root) / "recomp/cargo"
    subprocess.run(["cargo", "build", "--release", "--quiet", "-p", "rk-analysis",
                    "--manifest-path", str(ROOT / "translator/Cargo.toml"), "--target-dir", str(target)],
                   check=True)
    return target / "release" / ("rk-analysis.exe" if os.name == "nt" else "rk-analysis")


def analysis_input(cfg):
    value = translation_input(cfg)
    value["analysis"] = cfg.get("analysis") or {}
    return value


def write_analysis_input(cfg, build_root):
    path = Path(build_root) / "recomp/analysis-input.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(analysis_input(cfg), indent=1, sort_keys=True) + "\n"
    if not path.is_file() or path.read_text() != text:
        path.write_text(text)
    return path


def run_analysis(args, cfg, build_root):
    """Generate a fresh facts candidate with the configured discovery policy."""
    observations = list(args.analysis_observation)
    command = [str(rust_analysis(build_root)),
               "--input", str(write_analysis_input(cfg, build_root)),
               "--out", str(args.produce_analysis_facts.resolve())]
    for observation in observations:
        command += ["--observation", str(observation.resolve())]
    if args.analysis_report:
        command += ["--report", str(args.analysis_report.resolve())]
    if args.analysis_compare:
        command += ["--compare", str(args.analysis_compare.resolve())]
    subprocess.run(command, cwd=ROOT, check=True)


def rust_translator(build_root):
    """Return the bundled translator or build it into the build root."""
    bundled = ROOT.parent / "dependencies/bin/recomp-translate"
    if (ROOT.parent / "dependencies/manifest.json").is_file():
        if not bundled.is_file():
            raise ValueError("Bundled Rust translator is missing; reinstall Recomp Toolkit")
        return bundled
    target = Path(build_root) / "recomp/cargo"
    subprocess.run(["cargo", "build", "--release", "--quiet", "--locked", "-p", "recomp-translate",
                    "--manifest-path", str(ROOT / "translator/Cargo.toml"), "--target-dir", str(target)],
                   check=True)
    return target / "release" / ("recomp-translate.exe" if os.name == "nt" else "recomp-translate")


def test_translator_analysis(build_root):
    subprocess.run(["cargo", "test", "--quiet", "--locked", "--lib", "-p", "rk-core", "-p", "rk-ssa", "-p", "rk-global",
                    "--manifest-path", str(ROOT / "translator/Cargo.toml"),
                    "--target-dir", str(Path(build_root) / "recomp/cargo")], check=True)


def run_reloc_extractor(cfg):
    """Refresh the game-owned reloc manifest through its configured extractor."""
    reloc = cfg["reloc"]
    command = [sys.executable, str(reloc["extractor"]),
               "--install", str(cfg["developer_exe_path"].parent),
               "--exe", str(cfg["developer_exe_path"]),
               "--out", str(reloc["manifest_path"])] + reloc["args"]
    subprocess.run(command, cwd=ROOT, check=True)


def write_translation_input(cfg, build_root):
    path = Path(build_root) / "recomp/translate-input.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(translation_input(cfg), indent=1, sort_keys=True) + "\n"
    if not path.is_file() or path.read_text() != text:
        path.write_text(text)
    return path


def rust_translation_fingerprints(cfg, build_root):
    binary = rust_translator(build_root)
    common = hashlib.sha256(b"recomp-translate-rust-v1\n")
    common.update(json.dumps(translation_input(cfg), sort_keys=True).encode())
    common.update(binary.read_bytes())
    for path in sorted((ROOT / "tools").glob("*game_config.py")):
        common.update(path.name.encode() + b"\0" + path.read_bytes())
    images = ["main"] + [module["key"] for module in cfg["aux_modules"]]
    fingerprints = {}
    for key in images:
        h = common.copy()
        h.update(b"\0" + key.encode())
        h.update(rust_translation_dependencies(cfg, key))
        for name in sorted(code_map_files(cfg, key)):
            h.update(name.name.encode() + b"\0" + name.read_bytes())
        fingerprints[key] = h.hexdigest()
    return fingerprints


def rust_translation_dependencies(cfg, key):
    """Hash external Rust inputs and the generation switches that affect output."""
    translate = cfg["translate"]
    digest = hashlib.sha256()
    if key == "main":
        for name in ("x87_facts", "x87_mode_metadata", "analysis_facts", "indirect_targets", "discovery_evidence"):
            value = translate.get(name)
            digest.update(name.encode() + b"\0")
            if value:
                path = Path(cfg["dir"]) / value
                digest.update(path.as_posix().encode() + b"\0" + path.read_bytes())
            else:
                digest.update(b"<unset>")
            digest.update(b"\0")

        native = translate.get("native") or {}
        header = native.get("header")
        digest.update(b"native-header\0")
        if header:
            root = Path(cfg["dir"]) / header
            pending = [(root, True)]
            seen = set()
            while pending:
                path, required = pending.pop()
                if path in seen:
                    continue
                seen.add(path)
                try:
                    data = path.read_bytes()
                except OSError:
                    if required:
                        raise
                    continue
                digest.update(path.as_posix().encode() + b"\0" + data + b"\0")
                parent = path.parent
                for line in data.decode(errors="replace").splitlines():
                    line = line.lstrip()
                    if not line.startswith("#"):
                        continue
                    rest = line[1:].lstrip()
                    if not rest.startswith("include"):
                        continue
                    include = rest[len("include"):].strip()
                    if include.startswith('"') and '"' in include[1:]:
                        pending.append((parent / include[1:].split('"', 1)[0], False))

        if cfg.get("reloc"):
            reloc = cfg["reloc"]
            digest.update(b"reloc-extractor\0" + reloc["extractor"].as_posix().encode() + b"\0")
            digest.update(reloc["extractor"].read_bytes() + b"\0")
            digest.update(json.dumps(reloc["args"], sort_keys=True).encode() + b"\0")
            manifest = reloc["manifest_path"]
            digest.update((manifest.read_bytes() if manifest.is_file() else b"<missing>") + b"\0")

    digest.update(b"rust-generation-env\0")
    for name in ("RK_NO_DESCRIPTORS", "RK_NO_DIRECT", "RK_X87_DYNAMIC", "RK_NO_ROOTS"):
        digest.update(name.encode() + b"=" + (b"1" if os.environ.get(name) is not None else b"0") + b"\0")
    return digest.digest()


def code_map_files(cfg, key):
    if key == "main":
        root, exe = cfg["code_map_path"], cfg["developer_exe_path"]
    else:
        module = next(module for module in cfg["aux_modules"] if module["key"] == key)
        root, exe = module["code_map_path"], module["path"]
    files = [path for path in Path(root).iterdir() if path.is_file()] if root and Path(root).is_dir() else []
    return files + [Path(exe)]


def translation_fingerprints(cfg, build_root=None):
    if cfg.get("translate_engine") == "rust":
        return rust_translation_fingerprints(cfg, build_root)
    return python_translation_fingerprints(cfg)


def python_translation_fingerprints(cfg):
    """Per-image hashes of everything that can change that image's translated sources.

    Each image owns its executable, code map and translation settings. Runtime
    layout values such as the heap range are absent: they steer the build, not
    the generated C. Adding an auxiliary module therefore leaves the main
    image's fingerprint alone, and a partial regeneration updates only the
    images it actually emits."""
    from emission_cache import identity, source_revisions
    from settings import Settings

    def stable(value):
        if isinstance(value, Path):
            return value.as_posix()
        if isinstance(value, bytes):
            return value.hex()
        if isinstance(value, dict):
            return {key: stable(item) for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [stable(item) for item in value]
        return value

    translation_game = {key: cfg["game"][key] for key in ("image_base", "entry_point", "sha256")}
    config = dict(cfg)
    config["game"] = translation_game
    images = {"main": None}
    images.update({module["key"]: module["key"] for module in cfg["aux_modules"]})
    finalization = json.dumps(source_revisions()["finalization"], sort_keys=True).encode()
    fingerprints = {}
    for key, module in images.items():
        settings = Settings(config, module=module)
        settings.module = stable(settings.module)
        h = hashlib.sha256(b"recomp-translate-v3\n" + finalization + b"\0" + identity(settings).encode())
        for path in (ROOT / "tools/game_config.py", ROOT / "tools/gen_game_config.py"):
            h.update(path.relative_to(ROOT).as_posix().encode() + b"\0" + path.read_bytes())
        fingerprints[key] = h.hexdigest()
    return fingerprints


STAMP_FORMAT = 2


def read_translation_stamp(path):
    try:
        stamp = json.loads(path.read_text())
    except (OSError, ValueError):
        return {}
    if stamp.get("format") != STAMP_FORMAT or not isinstance(stamp.get("images"), dict):
        return {}
    return {key: value for key, value in stamp["images"].items()
            if isinstance(key, str) and isinstance(value, str)}


def write_translation_stamp(path, images):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".new")
    temporary.write_text(json.dumps({"format": STAMP_FORMAT, "images": dict(sorted(images.items()))},
                                    indent=2) + "\n")
    temporary.replace(path)


def image_generated(build_root, key):
    gen = Path(build_root) / "recomp/gen"
    return (gen / "table.c").is_file() if key == "main" else (gen / ("aux-" + key)).is_dir()


def publish_generated(build_root, translate, partial=False, main=True):
    """Stage a translation, then publish it without disturbing unchanged files.

    `translate(stage_dir)` writes the sources and raises on failure; the
    published tree is untouched in that case. On success the stage is synced
    into gen/ file by file, under the build lock, so a reader never sees half
    a generation and ninja only rebuilds what actually changed. A partial
    publish only touches the directories the stage carries, and, when `main`,
    replaces the top-level generated files instead of deleting unmentioned
    directories."""
    recomp = Path(build_root) / "recomp"
    recomp.mkdir(parents=True, exist_ok=True)
    gen = recomp / "gen"
    stage = recomp / ("gen.new.%d" % os.getpid())
    shutil.rmtree(stage, ignore_errors=True)
    stage.mkdir()
    try:
        translate(stage)
        # x86.h sits beside the generated sources so #include "x86.h" resolves.
        shutil.copy(ROOT / "runtime/x86.h", stage / "x86.h")
        if not partial:
            sync_tree(stage, gen)
        else:
            for entry in sorted(stage.iterdir()):
                if entry.is_dir():
                    sync_tree(entry, gen / entry.name)
            if main:
                stage_files = {p.name for p in stage.iterdir() if p.is_file()}
                for path in list(gen.iterdir()):
                    if path.is_file() and path.name not in stage_files:
                        path.unlink()
                for path in stage.iterdir():
                    if path.is_file():
                        sync_file(path, gen / path.name)
    finally:
        shutil.rmtree(stage, ignore_errors=True)
    symbols = gen / "symbols.json"
    if symbols.is_file():
        temporary = recomp / ("symbols.json.new.%d" % os.getpid())
        shutil.copy(symbols, temporary)
        temporary.replace(recomp / "symbols.json")


def harness_name(system=None):
    return "harness.dylib" if (system or platform.system()) == "Darwin" else "harness.so"


def build_translator_harness(gen, out):
    """Compile a synthetic translation and the diff host without a game build."""
    gen = Path(gen).resolve()
    generated = sorted(gen.glob("chunk_*.c")) + [gen / "table.c", gen / "call_facts.c"]
    if not (gen / "table.c").is_file():
        sys.exit("%s/table.c is missing: translate the fixture first" % gen)
    host = [ROOT / "tools/recomp/tests/harness.c", ROOT / "tools/recomp/diff/host.c",
            ROOT / "platform/os_posix.cpp", ROOT / "platform/env.cpp",
            ROOT / "tools/recomp/diff/host.cpp", ROOT / "runtime/guest_strings.cpp",
            ROOT / "runtime/dispatch.cpp"]
    out = Path(out).resolve()
    objects = out.parent / (out.stem + "-objects")
    objects.mkdir(parents=True, exist_ok=True)
    darwin = platform.system() == "Darwin"
    cc = ["xcrun", "clang"] if darwin else ["clang"]
    built = []
    for index, source in enumerate(generated):
        obj = objects / ("gen-%03d.o" % index)
        subprocess.check_call(cc + ["-std=c11", "-O1", "-ffp-contract=off", "-w", "-fPIC",
                                    "-I", str(gen), "-I", str(ROOT), "-I", str(ROOT / "runtime"),
                                    "-c", str(source), "-o", str(obj)])
        built.append(str(obj))
    for source in host[2:]:
        obj = objects / (source.stem + ".o")
        subprocess.check_call(cc + ["-x", "c++", "-std=c++17", "-O1", "-fno-exceptions", "-fPIC",
                                    "-I", str(ROOT), "-c", str(source), "-o", str(obj)])
        built.append(str(obj))
    whole = (["-dynamiclib", "-lc++", "-framework", "CoreFoundation", "-framework", "CoreServices"]
             if darwin else ["-shared", "-lm"])
    subprocess.check_call(cc + ["-O1", "-ffp-contract=off", "-std=c11", "-w", "-fPIC",
                                "-I", str(gen), "-I", str(ROOT), "-I", str(ROOT / "runtime"),
                                str(host[0]), str(host[1])] + built + whole + ["-o", str(out)])
    print("built translator harness: %s" % out)


def build_diff_harness(build_root, out):
    """Compile the Unicorn diff harness beside the translated archive.

    It links the archive whole so its address-indexed entry tables survive, and
    the OS shim so its guest arena is mapped. The archive itself is only read;
    tools/build.py --regenerate owns it. Reuses the library when every input is
    older, so a repeated run does not relink."""
    archive = archive_path(build_root)
    gen = Path(build_root) / "recomp/gen"
    if not archive.exists():
        sys.exit("%s is missing: build the game first" % archive)
    # Desktop macOS splits the main translation's bodies into dylibs beside the
    # archive; the harness links those instead of a monolithic archive.
    cache = archive.parent.parent / "CMakeCache.txt"
    split = cache.is_file() and "RECOMP_SPLIT_TRANSLATION:BOOL=ON" in cache.read_text().splitlines()
    pieces = sorted(archive.parent.parent.glob("librecomp_piece_*.dylib")) if split else []
    sources = [ROOT / "tools/recomp/tests/harness.c", ROOT / "tools/recomp/diff/host.c",
               ROOT / "platform/os_posix.cpp", ROOT / "platform/env.cpp",
               ROOT / "tools/recomp/diff/host.cpp", ROOT / "runtime/guest_strings.cpp",
               ROOT / "runtime/dispatch.cpp"]
    inputs = sources + [Path(__file__), archive, ROOT / "runtime/x86.h", gen / "table.c"] + pieces
    if cache.is_file():
        inputs.append(cache)
    if out.exists() and out.stat().st_mtime > max(p.stat().st_mtime for p in inputs):
        print("diff harness up to date: %s" % out)
        return
    out.parent.mkdir(parents=True, exist_ok=True)
    darwin = platform.system() == "Darwin"
    cc = ["xcrun", "clang"] if darwin else ["clang"]
    objects = []
    for source in sources[2:]:
        obj = out.with_name(source.stem + ".o")
        subprocess.check_call(cc + ["-x", "c++", "-std=c++17", "-O1", "-fno-exceptions", "-fPIC",
                                    "-I", str(ROOT), "-c", str(source), "-o", str(obj)])
        objects.append(str(obj))
    whole = (["-dynamiclib", "-Wl,-force_load," + str(archive)] + [str(p) for p in pieces] + [
               "-Wl,-rpath," + str(archive.parent.parent), "-lc++",
               "-framework", "CoreFoundation", "-framework", "CoreServices"] if darwin else
             ["-shared", "-Wl,--whole-archive", str(archive), "-Wl,--no-whole-archive", "-lm"])
    subprocess.check_call(cc + ["-O1", "-ffp-contract=off", "-std=c11", "-w", "-fPIC",
                                "-I", str(gen), "-I", str(ROOT), "-I", str(ROOT / "runtime"),
                                str(sources[0]), str(sources[1])] + objects + whole +
                          ["-o", str(out)])
    print("built diff harness: %s" % out)


def run_rust_translator(stage, cfg, build_root, aux_modules=(), main=True, reloc_only=False):
    binary = rust_translator(build_root)
    source = write_translation_input(cfg, build_root)
    if reloc_only:
        command = [str(binary), "--input", str(source), "--out", str(stage),
                   "--report", str(Path(build_root) / "recomp/translate-reloc-report.json"),
                   "--reloc-only"]
        subprocess.run(command, cwd=ROOT, check=True)
        return
    images = ([(None, Path(stage))] if main else []) + [
        (module["key"], Path(stage) / ("aux-" + module["key"])) for module in aux_modules]
    for key, destination in images:
        destination.mkdir(parents=True, exist_ok=True)
        report = "recomp/translate-report.json" if key is None else "recomp/translate-%s-report.json" % key
        command = [str(binary), "--input", str(source), "--out", str(destination),
                   "--report", str(Path(build_root) / report)]
        if key is not None:
            command += ["--module", key]
        subprocess.run(command, cwd=ROOT, check=True)


def run_translator(stage, game_dir, build_root, aux_modules=(), regen_functions=(), main=True,
                   cache_emission=True, cfg=None, reloc_only=False):
    if cfg is not None and cfg["translate_engine"] == "rust":
        return run_rust_translator(stage, cfg, build_root, aux_modules, main, reloc_only)
    if main:
        command = [sys.executable, str(ROOT / "tools/recomp/driver.py"), "--out", str(stage),
                   "--game", str(game_dir),
                   "--report", str(Path(build_root) / "recomp/translate-report.json")]
        if cache_emission or regen_functions:
            command += ["--emission-cache", str(Path(build_root) / "recomp/emission-cache")]
        for function in regen_functions:
            command += ["--regen-function", function]
        subprocess.run(command, cwd=ROOT, check=True)
    for module in aux_modules:
        destination = Path(stage) / ("aux-" + module["key"])
        destination.mkdir()
        command = [sys.executable, str(ROOT / "tools/recomp/driver.py"), "--out", str(destination),
                   "--game", str(game_dir), "--module", module["key"],
                   "--report", str(Path(build_root) / ("recomp/translate-%s-report.json" % module["key"]))]
        subprocess.run(command, cwd=ROOT, check=True)


def web_site(game_dir, build_root, preset, cfg):
    """The launcher page with this game's web build beside it, ready to serve."""
    site = Path(build_root) / (preset + "-site")
    web_launcher.build([game_dir], site)
    web_launcher.copy_web_build(cfg["game"]["id"], Path(build_root) / preset / "recomp", site)
    return site


def parse_args(argv, system=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--regenerate", nargs="?", const="all", default=None, metavar="MODULES",
                        help="Regenerate translated C: all images, or only the named auxiliary modules "
                             "(comma-separated; 'main' names the game image, 'reloc' the relocatable templates)")
    parser.add_argument("--engine", choices=("python", "rust"), default=None,
                        help="Translator to regenerate with (default: [translate] engine, else rust)")
    parser.add_argument("--no-emission-cache", action="store_true",
                        help="Disable prepared-body reuse and selected-function regeneration")
    parser.add_argument("--regen-function", action="append", default=[], metavar="HEX_ADDR",
                        help="Re-emit selected functions from validated whole-image preparation")
    parser.add_argument("--cache-emission", action="store_true",
                        help="Cache prepared graphs for incremental and selected-function regeneration (default)")
    parser.add_argument("--function-corpus", type=Path, metavar="MANIFEST",
                        help="Build/check/report a game-owned native-reference function corpus")
    parser.add_argument("--corpus-checks", type=int, default=4096)
    parser.add_argument("--corpus-asan", action="store_true",
                        help="Build the corpus with AddressSanitizer; correctness only (needs --corpus-trial-ms 0)")
    parser.add_argument("--corpus-trial-ms", type=float, default=10.0,
                        help="Time budget per eager trial in milliseconds; each row's call count is "
                             "calibrated once to fill it. Zero runs correctness/size only")
    parser.add_argument("--corpus-trials", type=int, default=9)
    parser.add_argument("--target", choices=sorted(TARGETS), default="app")
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 2, 8))
    parser.add_argument("--translation-piece-mb", type=int,
                        help="Generated C source MB per macOS translation dylib (CMake default: 160)")
    parser.add_argument("--preset", default=default_preset(system), help="CMake configure preset")
    parser.add_argument("--config", choices=("Release", "Debug"), default="Release")
    parser.add_argument("--package-toolkit", action="store_true")
    parser.add_argument("--toolkit-python-root", type=Path)
    game_config.add_game_args(parser)
    parser.add_argument("--stub", action="store_true",
                        help="Link the hosts against a stub translation (no game code; CI's build)")
    parser.add_argument("--no-pcode", action="store_true",
                        help="Omit the runtime SLEIGH lifter and p-code interpreter; missing code remains fatal")
    parser.add_argument("--no-observe", action="store_true",
                        help="Compile out indirect-transfer observation")
    parser.add_argument("--check-call-facts", action="store_true",
                        help="Build with RECOMP_CHECK_CALL_FACTS: abort on indirect calls outside the translator's target facts and on imports that change the x87 control word")
    parser.add_argument("--produce-analysis-facts", type=Path, metavar="PATH",
                        help="build/run the offline analysis producer into PATH without merging existing facts")
    parser.add_argument("--analysis-observation", type=Path, action="append", default=[], metavar="TSV",
                        help="observation file for --produce-analysis-facts (repeatable; defaults to [analysis] observations)")
    parser.add_argument("--analysis-report", type=Path, help="producer report for --produce-analysis-facts")
    parser.add_argument("--analysis-compare", type=Path, help="old facts for a separate comparison report")
    parser.add_argument("--diff-harness", action="store_true",
                        help="Compile the Unicorn diff harness for the translated archive and exit")
    parser.add_argument("--translator-harness", action="store_true",
                        help="Compile a standalone fixture translation and the Unicorn diff host and exit")
    parser.add_argument("--translator-gen", type=Path, default=None,
                        help="Generated sources for --translator-harness")
    parser.add_argument("--harness-out", type=Path, default=None,
                        help="Where --diff-harness or --translator-harness writes the shared library")
    parser.add_argument("--device", default=None, help="devicectl identifier (iOS) or adb serial (Android)")
    parser.add_argument("--team", default=os.environ.get("RECOMP_IOS_TEAM", ""),
                        help="Apple team id for automatic signing (ios target; default $RECOMP_IOS_TEAM)")
    parser.add_argument("--no-install", action="store_true", help="Build the mobile app without installing it")
    parser.add_argument("--push-game", action="store_true",
                        help="Android: push the configured game install minus [bundle].exclude before launch")
    parser.add_argument("--console", action="store_true", help="After launching on the device, stream its console")
    args = parser.parse_args(argv)
    if args.translator_harness:
        if not args.translator_gen or not args.harness_out:
            parser.error("--translator-harness requires --translator-gen and --harness-out")
        if any((args.stub, args.regenerate, args.function_corpus, args.diff_harness,
                args.produce_analysis_facts, args.config != "Release")):
            parser.error("--translator-harness is isolated; it cannot be combined with other build modes")
        return args, parser
    if args.produce_analysis_facts and any((args.regenerate, args.regen_function, args.diff_harness,
                                          args.function_corpus, args.stub, args.target != "app")):
        parser.error("--produce-analysis-facts is an isolated offline mode")
    if not args.produce_analysis_facts and any((args.analysis_observation, args.analysis_report, args.analysis_compare)):
        parser.error("--analysis-* options require --produce-analysis-facts")
    args.cache_emission = args.cache_emission or not args.no_emission_cache
    if not args.cache_emission and args.regen_function:
        parser.error("--regen-function requires the emission cache")
    if args.push_game and (args.target != "android" or args.no_install):
        parser.error("--push-game requires --target android without --no-install")
    if args.target == "ios" and not args.stub and not args.team:
        parser.error("--target ios needs --team or RECOMP_IOS_TEAM")
    if args.stub and (args.config == "Debug" or args.regenerate):
        parser.error("--stub cannot be combined with --config Debug or --regenerate")
    game_config.resolve_game_args(args)
    if not (args.game_dir / "game.toml").is_file():
        parser.error("No game config at %s/game.toml" % args.game_dir)
    if args.target == "plugins" and not (args.game_dir / "mods/CMakeLists.txt").is_file():
        parser.error("%s has no mods/CMakeLists.txt; nothing to build for --target plugins" % args.game_dir)
    if args.target in MACOS_ONLY and (system or platform.system()) != "Darwin":
        parser.error("The iOS packager runs on macOS")
    if args.target == "web" and not os.environ.get("EMSDK"):
        parser.error("--target web needs the Emscripten SDK's environment (source emsdk_env.sh)")
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.translation_piece_mb is not None and args.translation_piece_mb < 1:
        parser.error("--translation-piece-mb must be at least 1")
    if args.function_corpus and any((args.regenerate, args.stub,
                                      args.config != "Release", args.target != "app")):
        parser.error("--function-corpus is an isolated native Release build mode")
    if args.harness_out and not args.diff_harness:
        parser.error("--harness-out requires --diff-harness")
    if args.diff_harness and any((args.stub, args.regenerate, args.function_corpus,
                                 args.target != "app", args.config != "Release")):
        parser.error("--diff-harness is isolated; it cannot be combined with --stub, "
                     "--regenerate, --function-corpus, another target or Debug")
    if args.corpus_asan and not args.function_corpus:
        parser.error("--corpus-asan requires --function-corpus")
    if args.corpus_asan and args.corpus_trial_ms != 0:
        parser.error("--corpus-asan is a correctness build; pass --corpus-trial-ms 0")
    return args, parser


def regenerate_selection(value, cfg, parser):
    """None for a full regeneration, else the chosen image keys."""
    keys = [part.strip() for part in value.split(",") if part.strip()]
    if not keys or "all" in keys:
        return None
    known = {"main"} | {module["key"] for module in cfg["aux_modules"]}
    if cfg.get("reloc"):
        known.add("reloc")
    unknown = [key for key in keys if key not in known]
    if unknown:
        parser.error("--regenerate names unknown modules: %s (known: %s)"
                     % (", ".join(unknown), ", ".join(sorted(known))))
    return set(keys)


def main():
    """Check inputs, translate under the build lock when needed, then configure and build."""
    args, parser = parse_args(sys.argv[1:])
    if not args.function_corpus and not args.translator_harness:
        status = subprocess.call([sys.executable, str(ROOT / "tools/check_flag_state.py")])
        if status:
            sys.exit(status)
    if args.target == "extractors":
        from build_library import build_extractors
        build_extractors(args.build_root, args.config, innoextract=True)
        return
    if args.target == "toolkit":
        from build_library import build as build_library
        build_library(args.build_root, args.config, args.package_toolkit, args.toolkit_python_root)
        return
    if args.translator_harness:
        out = args.harness_out.resolve()
        with buildlock.BuildLock(out.parent, "tools/build.py --translator-harness"):
            build_translator_harness(args.translator_gen, out)
        return
    if args.diff_harness:
        out = args.harness_out or (args.build_root / "recomp/diff" / harness_name())
        with buildlock.BuildLock(args.build_root.parent, "tools/build.py --diff-harness"):
            build_diff_harness(args.build_root, out)
        return
    if args.function_corpus:
        from corpus.run import run_corpus
        with buildlock.BuildLock(args.build_root.parent, "tools/build.py --function-corpus"):
            run_corpus(args.function_corpus, args.game_dir, args.build_root / "function-corpus",
                       cmake_tool("cmake"), args.jobs, args.corpus_checks,
                       args.corpus_trial_ms, args.corpus_trials, asan=args.corpus_asan)
        return
    cfg = game_config.load(args.game_dir, args.build_root)
    cfg["translate_engine"] = args.engine or cfg["translate"].get("engine", "rust")
    if cfg.get("reloc") and cfg["translate_engine"] != "rust":
        parser.error("[reloc] requires the Rust translator; --engine python is unsupported")
    if args.produce_analysis_facts:
        with buildlock.BuildLock(args.build_root.parent, "tools/build.py --produce-analysis-facts"):
            run_analysis(args, cfg, args.build_root)
        return
    if args.regen_function and cfg["translate_engine"] == "rust":
        parser.error("--regen-function is not available with the Rust translator")
    if args.regen_function:
        args.regenerate = "all"
        if cfg["aux_modules"]:
            parser.error("selected regeneration currently requires an image without auxiliary modules")
    if args.regenerate and not cfg["developer_exe_path"].is_file():
        parser.error("Prepare your own game installation with tools/setup.py first")
    preset = preset_name(args.preset, args.config, stub=args.stub, target=args.target)
    build_dir = build_dir_for(args.build_root, preset)
    defines = game_defines(args.game_dir, args.build_root) + [
        "-DRECOMP_CHECK_CALL_FACTS=" + ("ON" if args.check_call_facts else "OFF"),
        "-DRECOMP_OBSERVE=" + ("OFF" if args.no_observe else "ON")]
    if args.translation_piece_mb is not None:
        defines.append("-DRECOMP_SPLIT_TRANSLATION=ON")
        defines.append("-DRECOMP_GEN_PIECE_BUDGET=" + str(args.translation_piece_mb * 1000000))
    try:
        # The lock lives at <build root>/recomp/.lock: BuildLock joins build/recomp/.lock onto its argument.
        with buildlock.BuildLock(args.build_root.parent, "tools/build.py"):
            if args.target in NEEDS_GEN and args.regenerate:
                if not cfg.get("code_map_path"):
                    parser.error("Translation needs the game's code map: set [translate] code_map in game.toml")
                selection = regenerate_selection(args.regenerate, cfg, parser)
                if cfg.get("reloc") and (selection is None or "main" in selection or "reloc" in selection):
                    run_reloc_extractor(cfg)
                fingerprints = translation_fingerprints(cfg, args.build_root)
                stamp = args.build_root / "recomp/translate.stamp"
                stamped = read_translation_stamp(stamp)
                translated = (args.build_root / "recomp/gen/table.c").is_file()
                if args.regen_function:
                    if not translated or selection is not None:
                        parser.error("--regen-function needs an existing translation and --regenerate all")
                    def regenerate_selected(stage):
                        shutil.copytree(args.build_root / "recomp/gen", stage, dirs_exist_ok=True)
                        run_translator(stage, args.game_dir, args.build_root,
                                       regen_functions=args.regen_function)
                    publish_generated(args.build_root, regenerate_selected)
                    stamped.pop("main", None)
                    write_translation_stamp(stamp, stamped)
                elif selection is None:
                    cache_ready = (cfg["translate_engine"] == "rust" or not args.cache_emission
                                   or (args.build_root / "recomp/emission-cache/index.pickle").is_file())
                    current = (set(stamped) == set(fingerprints)
                               and all(stamped[key] == value and image_generated(args.build_root, key)
                                       for key, value in fingerprints.items()))
                    if translated and cache_ready and current:
                        print("translation inputs unchanged; keeping %s/recomp/gen" % args.build_root)
                    else:
                        publish_generated(args.build_root,
                                          lambda stage: run_translator(stage, args.game_dir, args.build_root,
                                                                       aux_modules=cfg["aux_modules"],
                                                                       cache_emission=args.cache_emission,
                                                                       cfg=cfg))
                        write_translation_stamp(stamp, fingerprints)
                else:
                    if not translated:
                        parser.error("a partial --regenerate needs an existing translation; run --regenerate first")
                    if "reloc" in selection:
                        if selection != {"reloc"}:
                            parser.error("--regenerate reloc must be used on its own")
                        publish_generated(args.build_root,
                                          lambda stage: run_translator(stage, args.game_dir, args.build_root,
                                                                       main=False, reloc_only=True, cfg=cfg),
                                          partial=True, main=False)
                    else:
                        main_image = "main" in selection
                        aux = [module for module in cfg["aux_modules"] if module["key"] in selection]
                        publish_generated(args.build_root,
                                          lambda stage: run_translator(stage, args.game_dir, args.build_root,
                                                                       aux_modules=aux, main=main_image,
                                                                       cache_emission=args.cache_emission,
                                                                       cfg=cfg),
                                          partial=True, main=main_image)
                        stamped.update((key, fingerprints[key]) for key in selection if key in fingerprints)
                        write_translation_stamp(stamp, {key: value for key, value in stamped.items()
                                                        if key in fingerprints})
            # Generated sources include the adjacent runtime header. Refresh
            # it under the same lock even when their translation is unchanged.
            header = args.build_root / "recomp/gen/x86.h"
            if header.parent.is_dir():
                current = (ROOT / "runtime/x86.h").read_bytes()
                if not header.is_file() or header.read_bytes() != current:
                    temporary = header.with_suffix(".h.new")
                    temporary.write_bytes(current)
                    temporary.replace(header)
            if args.target == "gen":
                return
            pcode_disabled = (args.no_pcode or args.target in {"ios", "android", "web"} or
                              platform.system() == "Windows" or preset.startswith("windows-cross"))
            defines.append("-DRECOMP_PCODE_LIB=" + ("" if pcode_disabled else str(rust_pcode_library(args.build_root))))
            if args.target == "ios":
                if not args.stub and not (args.build_root / "recomp/gen/table.c").is_file():
                    parser.error("No translation in %s/recomp/gen; run tools/build.py --regenerate on macOS first"
                                 % args.build_root)
                configure(preset, defines + ["-DRECOMP_IOS_TEAM=" + args.team], build_dir=build_dir)
                extra = ["--", "CODE_SIGNING_ALLOWED=NO"] if args.stub else ["--", "-allowProvisioningUpdates"]
                build(preset, TARGETS["ios"], args.jobs, extra, build_dir=build_dir, config="Release")
                if not args.stub and not args.no_install:
                    app = ios_app_bundle(cfg["game"]["app_name"], args.build_root)
                    device = args.device or pick_device(devicectl_list())
                    install_and_launch(app, cfg["game"]["bundle_id"], device, args.console)
            else:
                configure(preset, defines, build_dir=build_dir)
                build(preset, TARGETS[args.target], args.jobs, build_dir=build_dir, config=args.config)
                if args.target == "web":
                    site = web_site(args.game_dir, args.build_root, preset, cfg)
                    print("Web site in %s (serve it with tools/web_launcher.py --serve)" % site)
                if args.target == "android":
                    apk = android_apk(args.build_root, cfg, gen_dir=build_dir, game_dir=args.game_dir)
                    if not args.no_install:
                        android_install_and_launch(apk, cfg["game"]["bundle_id"], args.device, args.console,
                                                   game_cfg=cfg if args.push_game else None,
                                                   build_root=args.build_root)
                system = "Windows" if preset.startswith("windows-cross") else platform.system()
                if args.target == "app" and not args.stub and system in {"Linux", "Windows"}:
                    # The desktop Ninja presets write OUTPUT_NAME into RECOMP_OUT.
                    desktop_root = args.build_root / "windows" if preset.startswith("windows-cross") else args.build_root
                    suffix = ".exe" if system == "Windows" else ""
                    binary = desktop_root / "recomp" / (cfg["game"]["app_name"] + suffix)
                    if not binary.is_file():
                        parser.exit(1, "No desktop app binary at %s after the build\n" % binary)
                    packaged = package_desktop.stage(binary, cfg, desktop_root / "package",
                                                     system=system, build_dir=build_dir, game_dir=args.game_dir)
                    print("Packaged %s" % packaged)
    except subprocess.CalledProcessError as error:
        parser.exit(error.returncode or 1, "Build failed; see the compiler output above.\n")
    except TimeoutError as error:
        parser.exit(1, "%s\n" % error)
    except (OSError, ValueError) as error:
        parser.exit(1, "%s\n" % error)


if __name__ == "__main__":
    main()
