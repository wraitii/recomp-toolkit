# Global Operations (2002) — development

## Build from source

This directory is the game's configuration root (`--game global-ops`). Prepare the game folder as in
the [README](README.md), then from the kit root:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
.venv/bin/python tools/setup.py --game global-ops --install "/path/to/Global Operations"
ulimit -s 65520
RUST_MIN_STACK=536870912 .venv/bin/python tools/build.py --game global-ops --regenerate --target app
open build/global-ops/GlobalOpsRecomp.app
```

The translator recurses deeply on this game's largest functions and overflows the default stack;
the `ulimit` and `RUST_MIN_STACK` settings above are required for `--regenerate`.

## Images

| Module | Base | Notes |
| --- | --- | --- |
| `Globalops.exe` | `0x003f0000` | Engine and renderer front end. Community 4.0 build; `.text` extended for the update's code |
| `globalops/cshell.dll` | `0x10000000` | Client game code: menus, HUD, server browser |
| `globalops/object.lto` | `0x10200000` | Server game objects (a DLL) |
| `globalops/CRes.dll` | `0x10300000` | Resources; also loaded as `SRes.dll` |
| `ltmsg.dll` | `0x10400000` | Engine messages |

The code maps (`metadata*/`) were exported from Ghidra 12.1 with each module rebased to the address
above. `discover_exclude` keeps the discovery sweep out of the 4.0 update's padding
(`0x3f1200`–`0x401000`), which decodes as garbage.

## Native replacements

| Source | Address | Purpose |
| --- | --- | --- |
| `native/startup.cpp` | `0040b880` | The update calls `GetEnvironmentVariableA` through mss32's import slot plus a fixed offset into the real `kernel32.dll`; the slot is pointed at the runtime's import for the call |
| `native/display.cpp` | `004c33b0` | `default.cfg` asks for 16-bit colour, which the D3D8 adapter does not offer; the request selects the 32-bit mode |
| `native/stack_probe.cpp` | `004b4a50` | MSVC 6 `_alloca_probe` leaves through `JMP EAX`; the replacement applies its register effects and returns |
| `native/voice.cpp` | `cshell 10098a20`, `object 10259f30` | Voice codec init calls an unchecked Miles ASI provider; voice is reported as not initialised |

`[environment]` presets `LT_ERROR=LT_MISSINGCLASS`; without it LithTech sets the variable and
relaunches itself through `CreateProcessA`.

## Game settings

- `[render] d3d8_wgpu`: the Rust Direct3D 8 renderer.
- `[media] bink_players = ["binkplay.exe"]`: the intro is played by `binkplay.exe`, which the game runs
  and waits for; the runtime plays the named movies in the game window instead.
- `[network] sockets = true`: Winsock on host sockets, for the server browser and online play.
- `[input] capture_hidden_cursor_only = true`: the game keeps a DirectInput mouse in the menus while
  showing the Windows cursor, so the host captures the pointer only while the cursor is hidden.

## Known issues

- Hosting launches `goserver.exe -s` (`cshell.dll` `0x10043790`) and quits. The dedicated server is a
  static-MFC application loading `server.dll`; it is not ported.
- An intermittent heap corruption late in long sessions.
- The diff harness does not link the startup replacement, which calls runtime functions.
