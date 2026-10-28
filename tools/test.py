#!/usr/bin/env python3
"""Run explicit contributor suites, with game-backed checks isolated from player saves.

The game is --game <id> or --game-dir (default the kit's stub game); outputs live
under the build root tools/build.py chooses for it."""

import argparse
import importlib.util
import os
from pathlib import Path
import platform
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/recomp"))
import buildlock  # noqa: E402

spec = importlib.util.spec_from_file_location("build_py", ROOT / "tools/build.py")
build_py = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_py)

def run(args, env=None):
    """Propagate a test command's failure instead of treating missing coverage as success."""
    subprocess.run([str(arg) for arg in args], cwd=ROOT, env=env, check=True)


def ctest(build_dir, labels, env):
    """Run the CTest entries whose label matches `labels` (a regex)."""
    run([build_py.cmake_tool("ctest"), "--test-dir", str(build_dir), "-L", labels, "--output-on-failure"], env)


def configure(preset, game_dir, build_root):
    build_dir = build_py.build_dir_for(build_root, preset)
    build_py.configure(preset, build_py.game_defines(game_dir, build_root), build_dir=build_dir)
    return build_dir


def native(preset, env, jobs, run_tests, game_dir, build_root):
    """Build every test binary this platform has; run the suites that need no snapshot."""
    build_dir = configure(preset, game_dir, build_root)
    build_py.build(preset, ["check_binaries"], jobs, build_dir=build_dir)
    if run_tests:
        ctest(build_dir, "nogame|game|gpu|device", env)


def mods_targets():
    """The game-independent mod API suite."""
    return ["mods_tests"]


def mods(preset, env, jobs, game_dir, build_root):
    """Build and run the game-backed mod suites under one build lock."""
    with buildlock.BuildLock(build_root.parent, "mod tests"):
        build_dir = configure(preset, game_dir, build_root)
        build_py.build(preset, mods_targets(), jobs, build_dir=build_dir)
        ctest(build_dir, "mods", env)


def without_switches(env):
    """Drop inherited runtime switches while preserving game and tool paths."""
    tool_keys = {"RECOMP_GAME_DIR", "RECOMP_BUILD_ROOT", "RECOMP_DEVELOPER_GAME_DIR", "RECOMP_DEVELOPER_EXE",
                 "RECOMP_IOS_TEAM", "RECOMP_OVERRIDE_HEADER", "RECOMP_FUNCS_H", "RECOMP_NO_HOOKS",
                 "RECOMP_IMAGE_BASE"}
    return {key: value for key, value in env.items()
            if not key.startswith("RECOMP_") or key in tool_keys}


def main():
    """Choose portable, compile-only or game-backed suites and check their prerequisites."""
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--d3d8-wgpu", choices=["test", "probe"], help="Rust D3D8 model tests or headless GPU probe")
    group.add_argument("--native", action="store_true", help="Build and run the native suites")
    group.add_argument("--mods", action="store_true", help="Real game-backed mod tests")
    group.add_argument("--compile-only", action="store_true", help="Build the native test binaries only")
    group.add_argument("--translator", action="store_true", help="Run the synthetic translator cases")
    parser.add_argument("--case", action="append", default=[], help="Translator case name (repeatable)")
    parser.add_argument("--preset", default=build_py.default_preset())
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 2, 8))
    parser.add_argument("--iterations", type=int, default=200, help="Translator inputs per case")
    build_py.game_config.add_game_args(parser)
    raw = parser.parse_args()
    if raw.translator:
        build_py.test_translator_analysis(ROOT / "build")
        command = [sys.executable, str(ROOT / "tests/translator/run.py"),
                   "--iterations", str(raw.iterations), "--jobs", str(raw.jobs)]
        for case in raw.case:
            command += ["--case", case]
        run(command, without_switches(os.environ))
        return
    if raw.case:
        parser.error("--case requires --translator")
    args = build_py.game_config.resolve_game_args(raw)
    if not (args.game_dir / "game.toml").is_file():
        parser.error("No game config at %s/game.toml" % args.game_dir)
    cfg = build_py.game_config.load(args.game_dir, args.build_root)
    build_root = args.build_root
    game_backed = args.mods
    if game_backed and platform.system() != "Darwin":
        parser.error("Game-backed suites require macOS")
    if game_backed and not cfg["developer_exe_path"].is_file():
        parser.error("This suite needs your game installation; run tools/setup.py first, "
                     "or run `ctest --test-dir <build dir> -L nogame` for the portable suites")
    if game_backed and not build_py.archive_path(build_root, preset=args.preset).is_file():
        parser.error("Build the game with tools/build.py before running this suite")
    env = without_switches(os.environ)
    env["PY"] = sys.executable
    try:
        if args.d3d8_wgpu:
            rust_env = dict(env, CARGO_TARGET_DIR=str(build_root / "d3d8-wgpu/rust"))
            manifest = ROOT / "graphics/d3d8-wgpu/Cargo.toml"
            if args.d3d8_wgpu == "test":
                run(["cargo", "test", "--locked", "--lib", "--manifest-path", manifest], rust_env)
            else:
                run(["cargo", "run", "--locked", "--bin", "probe-headless", "--manifest-path", manifest], rust_env)
        elif args.mods:
            mods(args.preset, env, args.jobs, args.game_dir, build_root)
        elif args.native or args.compile_only:
            native(args.preset, env, args.jobs, args.native, args.game_dir, build_root)
        else:
            parser.error("Choose a suite: --native, --compile-only, --mods or --d3d8-wgpu")
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, RuntimeError, TimeoutError) as error:
        parser.exit(1, "Tests failed: %s\n" % error)


if __name__ == "__main__":
    main()
