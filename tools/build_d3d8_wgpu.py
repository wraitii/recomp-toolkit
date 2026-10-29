import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess


def native_target_args(target):
    if not target:
        return []
    rustc = os.environ.get("RUSTC", "rustc")
    version = subprocess.run([rustc, "-vV"], capture_output=True, text=True, check=True).stdout
    host = next((line.removeprefix("host: ") for line in version.splitlines()
                 if line.startswith("host: ")), None)
    if target != host:
        raise ValueError(f"renderer Cargo target {target} does not match Rust host {host}")
    return ["--target", target]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--crate", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--bridge-header", type=Path, required=True)
    parser.add_argument("--cxx-header", type=Path, required=True)
    parser.add_argument("--native-libs", type=Path, required=True)
    parser.add_argument("--cxx", required=True)
    parser.add_argument("--deployment-target")
    parser.add_argument("--target")
    args = parser.parse_args()
    try:
        target_args = native_target_args(args.target)
    except ValueError as error:
        parser.error(str(error))
    build_env = dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir), CXX=args.cxx)
    if args.deployment_target:
        build_env["MACOSX_DEPLOYMENT_TARGET"] = args.deployment_target
    process = subprocess.Popen(
        [args.cargo, "rustc", "--release", "--lib", "--locked", "--message-format=json",
         "--manifest-path", str(args.crate / "Cargo.toml"), *target_args, "--", "--print=native-static-libs"],
        env=build_env,
        stdout=subprocess.PIPE, text=True)
    outputs = {}
    library_paths = []
    package = None
    native_libs = None
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
        reason = message["reason"]
        if reason == "build-script-executed":
            outputs[message["package_id"]] = Path(message["out_dir"])
            for value in message.get("linked_paths", []):
                kind, separator, path = value.partition("=")
                if not separator:
                    path = kind
                elif kind not in ("native", "all"):
                    continue
                library_paths.append(Path(path).resolve().as_posix())
        elif reason == "compiler-artifact" and message["target"]["name"] == "d3d8_wgpu":
            package = message["package_id"]
        elif reason == "compiler-message":
            match = re.search(r"native-static-libs: (.*)", message["message"]["message"])
            if match:
                native_libs = match.group(1).strip()
            print(message["message"].get("rendered") or message["message"]["message"], end="", flush=True)
    status = process.wait()
    if status:
        parser.exit(status, "D3D8 renderer Cargo build failed\n")
    corpus = subprocess.run(
        [args.cargo, "build", "--release", "--locked", "--manifest-path",
         str(args.crate / "Cargo.toml"), "--bin", "shader-corpus", *target_args],
        env=build_env)
    if corpus.returncode:
        parser.exit(corpus.returncode, "D3D8 shader corpus CLI build failed\n")
    if package not in outputs:
        parser.exit(1, "Cargo did not report the D3D8 bridge output directory\n")
    if native_libs is None:
        parser.exit(1, "Rust did not report the bridge's native link dependencies\n")
    args.native_libs.parent.mkdir(parents=True, exist_ok=True)
    if os.name == "nt":
        native_libs += "".join(f'\n/LIBPATH:"{path}"' for path in library_paths)
    args.native_libs.write_text(native_libs + "\n")
    output_dir = outputs[package]
    generated = (
        (output_dir / "cxxbridge/include/rust/cxx.h", args.cxx_header),
    )
    for source, destination in generated:
        if not source.is_file():
            parser.exit(1, f"Cargo did not generate the required header: {source}\n")
        destination.parent.mkdir(parents=True, exist_ok=True)
        if not destination.is_file() or destination.read_bytes() != source.read_bytes():
            shutil.copyfile(source, destination)
    bridge_headers = list((output_dir / "cxxbridge/include").rglob("bridge.rs.h"))
    if len(bridge_headers) != 1:
        parser.exit(1, f"Cargo generated {len(bridge_headers)} CXX bridge headers, expected one\n")
    args.bridge_header.parent.mkdir(parents=True, exist_ok=True)
    if not args.bridge_header.is_file() or args.bridge_header.read_bytes() != bridge_headers[0].read_bytes():
        shutil.copyfile(bridge_headers[0], args.bridge_header)


if __name__ == "__main__":
    main()
