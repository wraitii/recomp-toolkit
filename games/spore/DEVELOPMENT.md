# Spore (2008) — development

The game directory is `recomp-kit/games/spore/` (`--game spore`). `metadata/`
holds the packed code map for `SporebinEP1/SporeApp.exe`.

## Installed layout

```
Spore.exe
SporebinEP1/SporeApp.exe
SporeBin/SporeApp.exe
Data/
DataEP1/
bp1content/
```

The installation requires `Data`, `DataEP1` and `bp1content`.

## Setup and build

From the game repository root:

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game spore --install "$PWD/SPORE"
tools/.venv/bin/python tools/build.py --regenerate
```

Instructions the translator cannot model trap at their own address if reached.

Setup checks the pinned executable against `game.toml`, validates the data
directories and links the installation as `build/original`. It refuses to
continue until `[translate] code_map` names a packed map.

## Installation registry

`game.toml` supplies the installation values from the GOG Collection's
`goggame-1948823323.script`; saved registry values override these defaults.
The guest root maps to `SporebinEP1`, with the content directories alongside it.
Startup `0x007e7db0` reads `spore\DataDir` through `0x006ab870`; without it,
the package path resolves to the executable directory. The separate config-path
fallback finds `../Data/Config`, but cannot supply packaged default preferences
or option definitions, leading to the `setOption` error at `0x007c8eee`.

## steam_api.dll

The GOG build ships a 15 KB `steam_api.dll` that is not a Steam client. Its
`SteamAPI_Init` always succeeds, `SteamAPI_Shutdown` does nothing, and
`SteamApps` returns an object whose ownership check accepts the game's app id.
`runtime/steam_api.cpp` is a deliberately marked mock of those three answers;
it logs a warning when `SteamAPI_Init` is reached and leaves any other
`steam_api` import a strict named failure.

## goggame-1948823323.dll

The GOG build loads this helper from `SporebinEP1`; it exports no functions and
is mapped as an auxiliary image for its `DllMain`. `metadata-goggame/` is a
bootstrap code map. Auxiliary mappings may push the heap start up, so this DLL
owns `0x10000000`..`0x1003a000` and the effective heap base becomes
`0x1003a000`.

## Direct3D 9

The `D3DXCompileShader` IAT at `0x011e2014` is called at `0x006fb5df`,
`0x006fec68`, `0x00713d5e`, `0x00713df8`, `0x011f6cd4` and `0x011f7aeb`.
Each supplies entry point `main`, null macros/includes/constant-table output,
and its profile from the guest; flags are `0x12`/`0x32`, `0x2` and `0`.
The native compiler is vkd3d-shader 2.1. `DIVERGENCE(original):` it emits
full-precision bytecode when `D3DXSHADER_PARTIALPRECISION` is set, which the
Direct3D 9 rules permit; `SKIPVALIDATION` still passes through vkd3d's HLSL
front end, so malformed source may fail compilation.

The streaming draw buffer at `0x00f66f30` passes its 16-bit index count as
`SizeToLock` at `0x00f66fda`, while doubling only the offset. It later stores
that count at `this+0x28`; `0x00f67020` divides it by three for triangle-list
submission. Writable buffer unlocks preserve changed bytes outside the declared
lock range, within the allocated buffer, so this original quirk does not leave
the second half of an index upload stale. Draw fetch bounds remain enforced.

## Video conversion

`native/video.cpp` replaces `0x00fce6c0..0x00fce729`, the intro's MMX
pixel-conversion row loop. In the October 20 09:43 profile it occupies 31% of
main-thread samples. The pinned executable's bytes match Ghidra; call site
`0x00fcec87` in `0x00fceb10` and texture upload `0x00fd9490` establish its
five stack arguments and role. It reads the original tables at `0x015b1818`,
`0x015b2018` and `0x015b2818`, preserving wrapping word addition, unsigned
byte saturation, final registers, MMX state, flags and stack writes. Keeping
loop state local removes per-instruction guest-state overhead. Set
`RECOMP_SPORE_VIDEO_ORIGINAL=1` before launch to use the translated original.

The original frame selector `0x00fc94b0`, called from `0x00fcb450`, discards
queued frames behind the movie clock; presentation FPS therefore does not
measure movie-frame advancement. Decoding and frame selection stay translated.
`DIVERGENCE(original):` a fault inside the native row loop does not publish
intermediate registers. Original-byte differential validation and playback
validation remain outstanding; the app has not been launched for this change.

## Combat target

`native/combat.cpp` wraps `0x00d67cd0`, the update of creature action
`0x02d852e6` (enter `0x00d67980`, exit `0x00d672a0`, table `0x01583f30`). Its
lost-target exit, `0x00d65980` returning 1 for a null `[creature+0xe7c]`, runs
only while the swing animation in `[state+0x30]` is absent or finished
(`0x00c0e420`). A target cleared mid-swing, such as on the player's death,
reaches `0x00d65be0` and faults at `0x00d65d23`. The game's crash reporter
then terminates the process (known community crash on death in Creature Stage).
`BUGFIX(original):` with a null target and a swing handle the wrapper takes
that exit whatever the animation state, reproducing it: `XMM0` cleared and
`0.0f` stored to `[state+0x1c]`, one `0x00c0e420` query, then `EAX=0`, `ECX`
the fifth argument and the flags of `XOR AL,AL`. The skipped `0x00c0efc0`,
`0x00c0efd0`, `0x00d993d0`, `0x00d99460` and `0x00d65980` calls have no side
effects for a null target. Other states, including a null target without a
swing, run the translated original. Set `RECOMP_SPORE_COMBAT_ORIGINAL=1` before
launch to always use it. The app has not been launched for this change.

## MSVCR90.dll

The executable imports 159 entries from the VS2008 C runtime. The PE entry is
`0x011e11e1` (`__security_init_cookie` `0x011e1588`, then
`WinMainCRTStartup` `0x011e0f21`). Startup calls `_initterm_e`
(`0x13eb09c`..`0x13eb0ac`), whose first callback `0x011e1100` performs
`__set_app_type`, `_encode_pointer`, `__p__fmode`, `__p__commode`,
`_adjust_fdiv`, `__setusermatherr` and `_configthreadlocale`; then `_initterm`
(`0x13cc80c`..`0x13eb098`) runs 31,266 C++ initializers; `_acmdln`
(`0x13cc324`) is read as a data import, and `_ismbblead` runs per command-line
byte before `WinMain` (`0xf489e0`).

`runtime/msvcr90.cpp` implements that startup set, the guest-heap `malloc`
family, strings and character classes. The table is registered strict, so any
other MSVCR90 name still stops the run with a named diagnostic instead of
returning 0. Known gaps: stdio (`__iob_func`, `printf` family), double-returning
math and conversion (`atof`, `strtod`, `floor`), C++ exception handling
(`__CxxFrameHandler3`, `_except_handler3`, `_except_handler4_common`,
`_CxxThrowException`), locale, and time/stat conversion.
