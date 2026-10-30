import argparse
import os
from pathlib import Path
import shutil
import subprocess
from cargo_build import build_static_library, native_target_args


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
    try:
        output_dir = build_static_library(args.cargo, args.crate, target_args, build_env,
                                          "d3d8_wgpu", args.native_libs)
    except RuntimeError as error:
        parser.exit(1, str(error) + "\n")
    corpus = subprocess.run(
        [args.cargo, "build", "--release", "--locked", "--manifest-path",
         str(args.crate / "Cargo.toml"), "--bin", "shader-corpus", *target_args],
        env=build_env)
    if corpus.returncode:
        parser.exit(corpus.returncode, "D3D8 shader corpus CLI build failed\n")
    if output_dir is None:
        parser.exit(1, "Cargo did not report the D3D8 bridge output directory\n")
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
