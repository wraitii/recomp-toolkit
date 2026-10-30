#!/usr/bin/env python3
import argparse
import os
from pathlib import Path
from cargo_build import build_static_library, native_target_args


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--crate", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--native-libs", type=Path, required=True)
    parser.add_argument("--target")
    parser.add_argument("--deployment-target")
    args = parser.parse_args()
    env = dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir))
    if args.deployment_target:
        env["MACOSX_DEPLOYMENT_TARGET"] = args.deployment_target
    try:
        build_static_library(args.cargo, args.crate, native_target_args(args.target), env,
                             "recomp_compositor", args.native_libs)
    except (RuntimeError, ValueError) as error:
        parser.exit(1, str(error) + "\n")


if __name__ == "__main__":
    main()
