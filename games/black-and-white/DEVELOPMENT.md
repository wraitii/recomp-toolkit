# Black & White — development

## Install with Recomp Toolkit

Apply Fan Patch 1.42 to your installed game before importing it. In
Recomp Toolkit, choose **Black & White** and select
the patched game folder, including its matching DLLs. Choose a destination and
select **Install**, then **Start game** when the build finishes.

The toolkit does not apply patches. Prepare installer or disc copies outside
the app, then select the resulting patched folder. The stock 1.2 executable is
not a supported play version.

## Build from source

The behavioral reference is `runblack.exe` from Fan Patch 1.42; stock 1.2 is
supporting evidence only. This directory is the configuration root
(`--game black-and-white`). It includes code maps and native replacements for
terrain cell construction/rendering and LOD settings, with switches back to the
translated originals. `render.d3d8_wgpu` selects the renderer used by the
Direct3D 7 path.

From the game repository root (the one containing `recomp-kit/`):

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game black-and-white --install <directory>
tools/.venv/bin/python tools/build.py --regenerate --target app
```

Setup links the installed game at `build/original`; it changes no game files.
Prerequisites are in [CONTRIBUTING](../../CONTRIBUTING.md#prerequisites). The
translator needs `tools/.venv`; the system Python lacks `pefile`.

Targets: `--target app` builds `build/BlackWhiteRecomp.app`; `--target headless`
builds the smoke binary `build/recomp/pop_headless`. Runtime and CMake edits need
only the `--target` build; add `--regenerate` only when a translation input
changed (listings, the game profile, code map or translator sources).

The executable and auxiliary modules decode from their packed maps.
A full regeneration translates all modules with no failures expected; per-module
counts land in `build/recomp/translate-*-report.json` (`functions_ok ==
functions_total`, `failures` and `unmodelled` empty).

Run paths, diagnostics, profiling flags and the game's status are in the game
repository's `docs/recompilation.md`.
