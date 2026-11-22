import json
import os
from pathlib import Path
import re
import subprocess


def rust_host_target():
    version = subprocess.check_output([os.environ.get("RUSTC", "rustc"), "-vV"], text=True)
    return next((line.removeprefix("host: ") for line in version.splitlines()
                 if line.startswith("host: ")), None)


def native_target_args(target):
    if not target:
        return []
    host = rust_host_target()
    if target != host and "android" not in target:
        raise ValueError(f"graphics Cargo target {target} does not match Rust host {host}")
    return ["--target", target]


def android_cross_env(env, target, cxx=None):
    if not target or "android" not in target:
        return env
    ndk = env.get("ANDROID_NDK_HOME")
    if not ndk:
        return env
    env = dict(env)
    prebuilt = Path(ndk) / "toolchains/llvm/prebuilt"
    host_dirs = sorted(prebuilt.iterdir()) if prebuilt.is_dir() else []
    if not host_dirs:
        return env
    bindir = host_dirs[0] / "bin"
    api = env.get("ANDROID_PLATFORM_API", "29")
    cc_bin = bindir / f"{target}{api}-clang"
    cxx_bin = bindir / f"{target}{api}-clang++"
    ar_bin = bindir / "llvm-ar"
    t_under = target.replace("-", "_")
    t_upper = t_under.upper()
    if cc_bin.is_file():
        env.setdefault(f"CC_{t_under}", str(cc_bin))
    if cxx_bin.is_file():
        env.setdefault(f"CXX_{t_under}", str(cxx_bin))
        env["CXX"] = str(cxx_bin)
        env.setdefault(f"CARGO_TARGET_{t_upper}_LINKER", str(cxx_bin))
    elif cxx:
        env.setdefault(f"CXX_{t_under}", cxx)
    if ar_bin.is_file():
        env.setdefault(f"AR_{t_under}", str(ar_bin))
    # Match CMakePresets.json ANDROID_STL=c++_static so link-cplusplus does not
    # add a DT_NEEDED dependency on unpackaged libc++_shared.so.
    env.setdefault("CXXSTDLIB", "c++_static")
    return env


def build_static_library(cargo, crate, target_args, env, name, native_libs_path, extra_cargo_args=()):
    process = subprocess.Popen(
        [cargo, "rustc", "--release", "--lib", "--locked", "--message-format=json",
         "--manifest-path", str(crate / "Cargo.toml"), *extra_cargo_args, *target_args, "--",
         "--print=native-static-libs"],
        env=env, stdout=subprocess.PIPE, text=True)
    outputs = {}
    package = None
    native_libs = None
    library_paths = set()
    for line in process.stdout:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            match = re.search(r"native-static-libs: (.*)", line)
            if match:
                native_libs = match.group(1).strip()
            else:
                print(line, end="", flush=True)
            continue
        reason = message.get("reason")
        if reason == "build-script-executed":
            outputs[message["package_id"]] = Path(message["out_dir"])
            for value in message.get("linked_paths", []):
                kind, separator, path = value.partition("=")
                if not separator:
                    path = kind
                elif kind not in ("native", "all"):
                    continue
                library_paths.add(Path(path).resolve().as_posix())
        elif reason == "compiler-artifact" and message["target"]["name"] == name:
            package = message["package_id"]
        elif reason == "compiler-message":
            diagnostic = message["message"]
            match = re.search(r"native-static-libs: (.*)", diagnostic["message"])
            if match:
                native_libs = match.group(1).strip()
            print(diagnostic.get("rendered") or diagnostic["message"], end="", flush=True)
    if process.wait():
        raise RuntimeError(f"{name} Cargo build failed (exit {process.returncode})")
    if native_libs is None:
        raise RuntimeError(f"Rust did not report {name} native link dependencies")
    if os.name == "nt":
        native_libs += "".join(f'\n/LIBPATH:"{path}"' for path in sorted(library_paths))
    native_libs_path.parent.mkdir(parents=True, exist_ok=True)
    native_libs_path.write_text(native_libs + "\n")
    return outputs.get(package)
