# 4x4 Evolution 2 — development

## Install with Recomp Toolkit

In Recomp Toolkit, choose **4x4 Evolution 2** and
select the game folder containing `4x42.exe`, `tridx8.dll` and `tridx8tl.dll`.
Choose a destination and select **Install**, then **Start game** when the build
finishes. The OpenGL renderer (`TRIGL.DLL`) is unsupported.

## Build from source

This directory is the configuration root (`--game 4x4-evo2`). `game.toml` records
the supported executable identity and translation settings; `metadata/` holds
the code map without instruction contents or names.

From the game repository root (the one containing `recomp-kit/`):

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game 4x4-evo2 --install "/absolute/path/to/your/installed/game"
tools/.venv/bin/python tools/build.py --regenerate
open build/Evo2Recomp.app
```

Setup checks the executable against `game.toml` and links the installation at
`build/original`; no game files are copied or changed. Prerequisites are in
[CONTRIBUTING](../../CONTRIBUTING.md#prerequisites). Later builds need only
`tools/.venv/bin/python tools/build.py`. Add `--regenerate` after changing a
translation input: the code map, the translator or a translation setting.
`--regenerate --target gen` generates translated C without building.

Standalone host settings and controller setup open without starting the game:

```sh
build/Evo2Recomp.app/Contents/MacOS/Evo2Recomp --configure
```

BUGFIX(original): `native/input.cpp` wraps the retained binding loader
(`0x004823d0`) to refresh the primary controller's cached button count after a
successful load, matching the original counter at `0x004839c0`. Initialization
(`0x00481760`) counts default bindings before loading saved ones; live traces
confirmed that restored D-pad slots 13–16 were excluded by gameplay's stale
count of 13 (`0x0048ac00`). `RECOMP_INPUT_FIXUP=0` retains original behavior.
D-pad-to-horn behavior was verified in-game. Other cached controller fields
and the original handling of holes in the binding table remain unchanged.

## Renderers

The original `tridx8tl.dll` and `Tridx8.dll` hardware renderers are translated
auxiliary modules. Their Direct3D 8 calls use the kit's GPU backend. The second
DLL is relocated to 0x11000000 because both originals prefer 0x10000000;
the guest arena includes both images. The saved `system/metal.ini` selects this
path with `useDirect3D=1` and `rendererDLLPath=tridx8tl.dll` (hardware TnL) or
`tridx8.dll`. `useDirect3D=0` retains the original software rasterizer.
Unregistered renderer DLLs fail to load and the original engine falls back to
software rendering. OpenGL (`TRIGL.DLL`) remains unsupported.

To reproduce the renderer maps, import and analyze each supported DLL in
Ghidra. Run `Evo2RebaseRenderer` before analyzing `Tridx8.dll`; it relocates both
the image and its HIGHLOW pointer operands. Inspect `SeedRelocationEntries`
in `dry` mode and apply the verified candidates to recover runtime initializer
and callback entries missed by auto-analysis, then analyze their callees.
Run `Evo2ExportCodeMap`
with an absolute destination and the DLL name as its second
argument. Pack each export with `code_map.py` into `metadata-tridx8tl` or
`metadata-tridx8`; the latter requires `--base 0x11000000`. Regenerate and
rebuild the windowed app afterward.

DIVERGENCE(original): `native/startup.cpp` wraps the startup options menu
(`0x00491d00`) to lock the renderer to Direct3D8 Hardware TnL and the color
depth to 32. The Color Depth and Renderer entries still appear but no longer
change those values or move the selection; the rest of the menu and saved
resolution are unchanged. Set `RECOMP_EVO2_GRAPHICS=0` to keep the original
behavior.

DIVERGENCE(original): the kit exposes no wave input/output devices and returns
device or handle errors for those APIs. DirectSound remains the audio path.
QueryPerformanceCounter uses synthetic microsecond ticks within the runtime's
millisecond clock, saturating after 999 increments in one millisecond. Segment
operations use the kit's flat model; selector validation remains a fidelity gap.

## Frame pacing and video memory

`native/frame_pacing.cpp` wraps the retained translated frame-completion function
at `0x00495d10`. Its hardware branch presents through `0x005c75d0`; the software
branch presents through `0x006029d0`. Both pump messages through `0x0060b7b0`.
Gameplay then samples `getTime` (`0x0060b020`) at `0x004639a7`, so the wait is
included in the original elapsed timestep. The main loop divides elapsed timer
ticks by 18 and then 65536; timer initialization uses 1179648 ticks per second.
These relationships and the hook entry bytes were checked against the supported
executable and Ghidra on port 8168.

DIVERGENCE(original): the default cap is 60 frames per second, using monotonic
deadlines and cooperative guest sleeps. The original rendering, message pump,
registers, flags and timing calculations remain translated. Set
`RECOMP_EVO2_FPS=0` to retain uncapped original behavior; a value from 1 through
1000 overrides the cap. Slow frames do not trigger catch-up bursts. High-rate
physics behavior remains an original fidelity limitation; no physics equations
or timer values are changed.

A separate VRAM hook is unnecessary for the translated hardware renderer.
`tridx8tl.dll` samples `IDirect3DDevice8::GetAvailableTextureMem` during mode
creation, retains the result at `0x1062a5d4`, and returns it unchanged from
`APIDLLgetVideoMemory` (`0x100036c0`). The kit reports a fixed 384 MiB, safely below
the signed 32-bit limit, rather than the host GPU's potentially multi-gigabyte
memory size. This already avoids oversized modern VRAM reports. The value is a
modeled budget, not a measurement or allocation limit; no 256 MiB game override
has been added.

## Code map

The code map is exported from Ghidra with the game's `Evo2ExportCodeMap.java`
script. From the game repository root, after deliberate analysis has produced
instruction boundaries (Ghidra API setup is in `../ghidra_scripts/ghidra-helpers.md`):

```sh
GHIDRA_API_BASE=http://127.0.0.1:8169 ./tools/ghidra_api \
  /script/run '{"name":"Evo2ExportCodeMap","args":["/ABS/PATH/analysis/code-map-export"]}'
tools/.venv/bin/python recomp-kit/tools/recomp/code_map.py --pack analysis/code-map-export \
  --exe build/original/4x42.exe --out recomp-kit/games/4x4-evo2/metadata
tools/.venv/bin/python tools/build.py --regenerate
```

The exporter checks the executable hash, architecture and image base and refuses
an existing output directory. Entries Ghidra cannot represent stay explicit in
`[translate] entry_points` in `game.toml`.
