# Grand Theft Auto 2 (1999) — development

## Build from source

Prepare the game folder as in the [README](README.md), then from the kit root:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
.venv/bin/python tools/setup.py --game gta2 --install "/path/to/GTA2"
.venv/bin/python tools/build.py --game gta2 --regenerate --target app
open build/gta2/GTA2Recomp.app
```

## Images

| Module | Base | Notes |
| --- | --- | --- |
| `gta2.exe` | `0x00400000` | The game |
| `d3ddll.dll` | `0x10000000` | Direct3D 6 renderer (`gbh_*`), loaded by name from `rendername` |
| `Dmavideo.dll` | `0x10100000` | DirectDraw video driver (`Vid_*`), loaded from `videoname` |

Both DLLs prefer bases below the arena's shims (`0x00e00000`, `0x01000000`); they are rebased
with their relocations before analysis, and the code maps describe the rebased images. GTA2 also
ships Glide and software renderers, which this port does not load.

## Native replacements

| Source | Address | Purpose |
| --- | --- | --- |
| `native/dxversion.cpp` | `0044db50` | The DirectX SDK's GetDXVersion probes DirectMusic for DirectX 6.1, which the caller requires; the kit has no DirectMusic, so 6.1 is recorded without the probe |
| `native/manager.cpp` | `004d0820` | Options launches GTA2 Manager and treats a failed launch as fatal; the game stays in the menu |

## Game settings

- `[render] classic = true`: frames are presented as GTA2 rendered them, from the guest surface,
  scaled to the window. GTA2 draws its menu backgrounds with DirectDraw blits around Direct3D text,
  and the Enhanced world/UI split misplaced those layers.
- `[registry]`: GTA2 Manager's first-run screen settings. `rendername` and `videoname` must
  include `.dll`: the game compares them with `softdll.dll` and `3dfx.dll`.

## Notes

- The intro (`data/movie/intro.bik`) plays through the Bink buffer API (`BinkBufferOpen` …
  `BinkBufferBlit`). GTA2's Bink 1.0 passes `0x04000000` to `BinkOpen` with a file name; that bit
  became `BINKFROMMEMORY` only later.
- `data/gtaudio/m.wav` is opened as a stream on the menus but is not in the freeware data; the
  original fails the same way.
