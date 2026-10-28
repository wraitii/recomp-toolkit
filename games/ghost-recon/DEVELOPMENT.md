# Tom Clancy's Ghost Recon (2001) — development

## Install with Recomp Toolkit

In Recomp Toolkit, choose **Tom Clancy's Ghost
Recon** and select your GOG 1.4 game folder or
`setup_tom_clancys_ghost_recon_1.4.0.0_(19232).exe`. Installer imports include
Desert Siege and Island Thunder and need `innoextract` 1.9 or later
(`brew install innoextract` on macOS). Choose a destination and select **Install**,
then **Start game** when the build finishes.

Ctrl+Option+M releases mouse capture. `RECOMP_FXAA=1` enables optional scene
antialiasing.

## Build from source

This directory is the game's configuration root (`--game ghost-recon`). The
installer recipe records the verified checksum, expanded size and relocation of
`__support/app/Data/Save/Script/default.ass` to `Data/Save/Script/default.ass`,
matching the installed copy. Extraction does not run setup or apply patches.

From the game repository root (the one containing `recomp-kit/`):

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game ghost-recon --install "/path/to/Ghost Recon"
tools/.venv/bin/python tools/build.py --regenerate
open build/GhostReconRecomp.app
```

Setup checks the executable against `game.toml` and links your installation; no
game files are copied or changed. Ghidra is not needed to build: the build decodes
the executable with the code map in `metadata/`. Prerequisites are in
[CONTRIBUTING](../../CONTRIBUTING.md#prerequisites). Later builds need only
`tools/.venv/bin/python tools/build.py`.

`tools/build.py` in the game repository forwards to the kit with `--game ghost-recon`
and outputs in `build/`; `--projection-cull-tests` and `--port-tab-tests` replay the
native replacements against their translated originals.

## Code map

Export from Ghidra, then pack into the kit (from the game repository root):

```sh
./tools/ghidra_api /script/run '{"name":"GhostReconExportCodeMap.java","args":["/ABS/PATH/analysis/code-map-export"]}'
tools/.venv/bin/python recomp-kit/tools/recomp/code_map.py --pack analysis/code-map-export \
  --exe build/original/GhostRecon.exe --out recomp-kit/games/ghost-recon/metadata
```

The map (`recomp-code-map-v3`) holds spans, jump tables, interior entries and
non-returning calls, addresses only. Fix gaps in Ghidra (`ApplyCodeMapFixes.java`
documents its operations), then re-export and repack; the translator model in
`recomp-kit/tools/recomp/program.py` fails by name on anything the map lacks.
The exporter refuses an existing destination. 
## Native replacements

| Source | Original addresses | Purpose / comparison switch |
| --- | --- | --- |
| `native/projection_cull` | `0081ae30`, `0081aa00` | Projection mesh culling and build; `RECOMP_PROJECTION_ORIGINAL=1` |
| `native/scene_post` | `0047c160` | Scene boundary and optional FXAA before the HUD |
| `native/port_tab` | `00650490`, `006a7fe0` and others | Options **Port** tab with a live FXAA toggle; see its README |
