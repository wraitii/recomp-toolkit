# Civilization IV: Beyond the Sword — development

## Install with Recomp Toolkit

In Recomp Toolkit, choose **Civilization IV:
Beyond the Sword** and select the complete Civilization IV folder containing
`Beyond the Sword/` and the shared base-game assets.

Supply the matching `msxml3.dll` beside `Civ4BeyondSword.exe`; the app checks its
exact version along with the other required DLLs. Follow the app's preparation
checklist for acquisition guidance. Wine is not needed to launch the game.
Choose a destination and select **Install**, then **Start game** when the build
finishes. Voice recording is not implemented.

## Build from source

This directory is the configuration root (`--game civ4-bts`): `game.toml` pins
`Civ4BeyondSword.exe`, `metadata*/` hold the maps for it and its auxiliary DLLs,
and `native/` holds replacements. `mods/core` is the game's core mod content.
The installation's `CvGameCoreDLL` source is supporting research evidence only;
verify behavior against the matching binaries.

From the game repository root (the one containing `recomp-kit/`):

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game civ4-bts --install "$PWD/Civilization IV Complete/Civ4/Beyond the Sword"
tools/.venv/bin/python tools/build.py --regenerate
```

Setup checks the executable against `game.toml` and links the installation at
`build/original`. Beyond the Sword also uses base Civ4 data from its parent
directory; the installation link preserves that layout. The pinned `msxml3.dll`
must be supplied manually; its stock-install provenance is unknown.
Prerequisites are in [CONTRIBUTING](../../CONTRIBUTING.md#prerequisites).
Later builds need only
`tools/.venv/bin/python tools/build.py`; add `--regenerate` after changing a
translation input.

## Voice capture

`native/voice_capture.c` replaces capture device selection (`0x008ebb20`), the
voice checkbox adapter (`0x008e1fa0`) and capture activation (`0x008e2010`).
DIVERGENCE(original): selection remembers the device while voice is off;
enabling voice creates it, disabling voice stops/releases it, and capture
activation is ignored while voice is off. `RECOMP_CIV4_LAZY_CAPTURE=0` selects
the translated originals.

Ghidra disassembly/decompilation of the pinned executable shows the constructor
(`0x008ec3c0`) selecting capture regardless of the profile, buffer creation
(`0x009f76d0`) calling `Start(1)`, and teardown (`0x009f69e0`) stopping/releasing
the buffer and device. The Python checkbox calls `setUseVoice` (`0x004f32e0`),
whose vtable slot `0x138` points to `0x008e1fa0`; playback state is at `+0x790`.
Deferred creation reapplies the cached capture volume (`+0x780`) after the
original selector copies playback volume into it. Device validation/errors
remain on the original path when enabled. Host DirectSound capture is still
unimplemented; this change avoids unused setup, and does not provide recording.
The profile getter also checks the setup mode, so its displayed checkbox is not
by itself proof of the underlying playback state.

## Code map

The main executable's map is exported from Ghidra with `ExportCodeMap.java`, which
takes the destination, image base and executable SHA-256 and is shared with the
auxiliary modules.
The Ghidra project is `ghidra/Civ4BTS.gpr`, imported with automatic analysis
disabled; decline the GUI's analysis prompt to keep that state. Add the shared `../ghidra_scripts/` folder to Script Manager, enable the ghidraLlm API,
and verify the program with `./tools/ghidra_api /program/info`. The default
server is port 8166; `GHIDRA_API_BASE` selects another. After deliberate analysis
has produced instruction boundaries:

```sh
./tools/ghidra_api /script/run '{"name":"ExportCodeMap.java","args":["/ABS/PATH/analysis/code-map-export","0x00400000","90dbdbdcbd6c631c9d575881d0241d9a2012b1a5957bacbe68212745da22174e"]}'
tools/.venv/bin/python recomp-kit/tools/recomp/code_map.py --pack analysis/code-map-export \
  --exe build/original/Civ4BeyondSword.exe --out recomp-kit/games/civ4-bts/metadata
tools/.venv/bin/python tools/build.py --regenerate
```

The exporters validate the executable hash, architecture and image base and
refuse an existing output directory.

### Auxiliary modules

`msxml3.dll` is BBT-optimized and keeps its constant data in `.text`, so its map
needs evidence from Microsoft's public PDB (GUID `08737D37-4241-46DD-9FD1-4F04B2CA28C9`,
age 2), kept at `analysis/pdb/msxml3.pdb`:

```sh
tools/.venv/bin/python recomp-kit/tools/recomp/pdb_evidence.py --exe build/original/msxml3.dll \
  --pdb analysis/pdb/msxml3.pdb --pdbutil "$(brew --prefix llvm)/bin/llvm-pdbutil" \
  --out analysis/msxml3-pdb-evidence.json
tools/.venv/bin/python recomp-kit/tools/recomp/bootstrap_map.py --exe build/original/msxml3.dll \
  --base 0x10a00000 --evidence analysis/msxml3-pdb-evidence.json \
  --out recomp-kit/games/civ4-bts/metadata-msxml3 --report analysis/bootstrap-msxml3-report.json
tools/.venv/bin/python recomp-kit/tools/recomp/bootstrap_map.py --exe build/original/zlib1.dll \
  --base 0x10c00000 --evidence analysis/zlib1-data-evidence.json \
  --out recomp-kit/games/civ4-bts/metadata-zlib1 --report analysis/bootstrap-zlib1-report.json
```

`zlib1.dll` embeds strings and tables in `.text`; its data ranges were checked by
hex dump and kept in `analysis/zlib1-data-evidence.json`.

### CvGameCoreDLL

`CvGameCoreDLL.dll` is mapped at guest base `0x10500000` (file base `0x10000000`).
Its Ghidra map is seeded from evidence, then exported and packed:

1. Import `Assets/CvGameCoreDLL.dll` into `ghidra/Civ4BTS.gpr` with automatic
   analysis off. Make it the active program.
2. Write `slots.tsv` from the PE file's type-3 base relocations (`rva` column,
   one line per slot). `RebaseAuxModule.java` moves the image to the guest base
   and relocates exactly these slots once, so Ghidra's own relocation pass is not used.
3. `RebaseAuxModule.java` with the SHA-256, `0x10000000`, `0x10500000` and `slots.tsv`.
   It turns every analyzer off first.
4. Seed functions with `SeedAuxEntries.java`: observed targets (`site target kind
   count desc` rows, one per line, `-` for none), then `audit`, then `apply`.
   It refuses to run while any analyzer is on and follows direct calls from
   what it creates.
5. For pointers found in `.rdata` that name seeded code, write a `slot target`
   TSV and run `LinkDataSlots.java` on it, so the code map records them as data
   entries.
6. Export with `ExportCodeMap.java` (`0x10500000`, the file's SHA-256), then pack:

```sh
tools/.venv/bin/python recomp-kit/tools/recomp/code_map.py --pack analysis/cvgamecore-export \
  --exe "Civilization IV Complete/Civ4/Beyond the Sword/Assets/CvGameCoreDLL.dll" \
  --out recomp-kit/games/civ4-bts/metadata-cvgamecore --base 0x10500000
tools/.venv/bin/python tools/build.py --regenerate cvgamecore --target app
```

The translator also runs a bootstrap sweep of the image for every module (`discover`
in `[translate]`, on by default). Sweep bodies outside the Ghidra map are provisional:
they are emitted as decoded regions, and a provisional jump into mapped code goes
through the runtime dispatch rather than a trap. Unsupported instructions trap
at their own address if reached.
