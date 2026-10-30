import json
import os
from pathlib import Path
import re
import subprocess


def native_target_args(target):
    if not target:
        return []
    version = subprocess.check_output([os.environ.get("RUSTC", "rustc"), "-vV"], text=True)
    host = next((line.removeprefix("host: ") for line in version.splitlines()
                 if line.startswith("host: ")), None)
    if target != host:
        raise ValueError(f"graphics Cargo target {target} does not match Rust host {host}")
    return ["--target", target]


def build_static_library(cargo, crate, target_args, env, name, native_libs_path):
    process = subprocess.Popen(
        [cargo, "rustc", "--release", "--lib", "--locked", "--message-format=json",
         "--manifest-path", str(crate / "Cargo.toml"), *target_args, "--", "--print=native-static-libs"],
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
