# Host layer

The host layer starts a translated guest, services the operating-system and
graphics calls that reach the host, and presents frames. It contains no game
addresses or game-specific UI. The executable, profile, symbols, and optional
native replacements come from the selected game's `game.toml` and build files.

## Hosts and source map

| Source | Role |
| --- | --- |
| `boot.{h,cpp}` | Shared image loading, runtime setup, guest entry, close handling, fault reporting, and watchdog. |
| `game_path.{h,cpp}`, `preferences.{h,cpp}`, `configure_launcher.cpp` | Game discovery, persisted host settings, and the Configure launcher. |
| `sdl/main.cpp`, `sdl/platform_ui_*` | SDL3 app window, event pump, game selection, and platform-specific window services. |
| `launcher/` | Desktop game library: scanning, importing, installing, and configuration UI. |
| `controls/` | On-screen controls, controller mapping, layout storage, and the interactive layout editor. |
| `headless_main.cpp` | Runs without a visible window and can write presented frames to disk. |
| `smoke_main.cpp` | Script-driven host for repeatable input and presentation capture. |
| `present.{h,cpp}`, `present_thread.cpp`, `compositor.{h,cpp}`, `frame_pacing.h`, `performance_overlay.{h,cpp}` | Pixel conversion, frame composition, pacing metrics, overlays, and presentation. |
| `ui_layer.{h,cpp}` | Reconstructs supported UI blits from recorded frame writes into owned RGBA pixels and masks, tracking elements across frames; the presenter scales this layer separately from the game image. |
| `d3d_render.{h,cpp}`, `gpu/` | Legacy host renderer, native device adapter, fake device, and wgpu host integration. |
| `input.{h,cpp}`, `input_gate.{h,cpp}`, `input_touch.{h,cpp}`, `input_script*`, `sdl/keymap.cpp` | Input translation, gating, touch gestures, guest notifications, and timed scripted input. |
| `audio/`, `audio.h` | Software audio mixer, SDL output, MIDI synthesis, offline rendering, and audio capture. |

`ui_layer` is a pixel reconstruction layer, not a menu system. It replays
supported 1:1 indexed and RGB565 copies from the frame's recorded operations
and owns the resulting pixels independently of guest surfaces while preserving
element identities across frames. The current blit record does not retain
source extents for stretched copies. Cross-format
copies and fills without usable format evidence also cannot be reconstructed
exactly; those cases need richer format metadata in the host ABI.

The host uses `gpu/gpu.h` through the native wgpu adapter. GPU execution and
surface presentation use the Rust renderer's shared wgpu context; layout and
composition use `graphics/compositor`, and `graphics/gpu` owns shared device
and queue handles. The SDL host owns the window. The CPU fake device remains
available for host checks. Metal is the first qualification target; other wgpu
backends require separate qualification.

The graphics crates share `graphics/Cargo.toml` and `graphics/Cargo.lock`.
CMake builds two archives: CPU composition/presenter FFI, and the renderer with
GPU composition enabled. Separate Cargo output directories keep these feature
sets distinct; consumers also use separate output directories. All share the same
pinned dependencies and native target settings.
Their bridge headers are private kit integration, with no stable external ABI.
Native builds require Rust even when guest GPU rendering is disabled. Cross
compilation fails explicitly until the complete graphics integration supports it.

Portable toolkit packaging stages both archives, native linker response files
and generated CXX headers, plus the vkd3d shader compiler library/public headers
and corresponding licenses/source. Staged graphics builds need neither Cargo nor Flex/Bison. Sources include tracked and non-ignored new files,
excluding removed files. The dependency manifest records the base Git commit,
dirty status, bundled source hashes and graphics artifact hashes. Build outputs,
profiles and game installations stay ignored. See [testing](../docs/testing.md)
for the final source and packaged-kit verification commands.

## Boot and scheduling

`boot_load()` initializes memory, loads the selected image, registers DirectX
shims, initializes the guest context, and installs the host clock and wait
callbacks. `boot_run()` enters the configured guest entry point. A host provides
callbacks for periodic work, idle waits, and run reporting through
`BootOptions`.

Guest code can run on cooperative worker threads. A host callback that touches
thread-affine window or GPU state must check `boot_on_run_thread()` and leave
that work to the run thread. When the guest blocks, `idle_wait` lets a windowed
host continue servicing events while the runtime waits for a signal. Closing
requests the guest's normal close path; the shared watchdog and unwind path
handle guests that do not return.

## Input, presentation, and audio

The SDL host maps platform events into the host input model, which then updates
the guest's DirectInput state and window messages. Pointer capture is managed
by the window host and released on focus loss or the configured capture
gesture (Ctrl+Alt+M on desktop). The headless and smoke hosts can use scripts
to deliver reproducible keyboard and pointer input.

The presenter expands indexed, RGB565, or supported packed-color pixels,
composes the game frame and host-owned overlays, and fits the result to the
drawable while preserving aspect ratio. The renderer keeps DirectX resource
and synchronization behavior at the host boundary; GPU command and resource
operations go through `gpu::Device` and its Rust wgpu adapter. Frames retain
texture leases through GPU completion; the input path consumes published layout
snapshots.

Audio calls feed a software mixer. The desktop output uses SDL, while the same
mixer can render offline for tests and captures. MIDI music is synthesized
through the configured SoundFont implementation. Headless execution can
capture audio without opening an output device.

For scripted runs, `recomp_smoke` accepts `--script <path>` (or
`RECOMP_SCRIPT=<path>`). `recomp_headless` and the desktop app accept a timed input
file through `RECOMP_INPUT_SCRIPT=<path>`. `RECOMP_FRAMES=<directory>` selects
the frame output directory, and `RECOMP_FRAME_EVERY=<n>` controls how often a
frame is saved. `RECOMP_MAX_FRAMES` and `RECOMP_MAX_SECONDS` set run limits;
`RECOMP_HOST_AUDIO_CAPTURE=<path.wav>` captures mixed audio.

## Build and test

Use the configured project Python and point commands at the directory containing
the target `game.toml`:

```sh
.venv/bin/python tools/build.py --game-dir /abs/path/to/game --target app
.venv/bin/python tools/build.py --game-dir /abs/path/to/game --target headless
.venv/bin/python tools/build.py --game-dir /abs/path/to/game --target smoke

.venv/bin/python tools/test.py --game-dir /abs/path/to/game --compile-only
.venv/bin/python tools/test.py --game-dir /abs/path/to/game --native
.venv/bin/python tools/test.py --game-dir /abs/path/to/game --mods
```

The app target builds the platform app. `headless` and `smoke` are available on
desktop builds. `--compile-only` builds native test binaries; `--native` also
runs the native runtime, host, and available GPU suites. `--mods` runs game-backed
mod tests and requires a built game archive and installed executable; it is
available on macOS. The build and test tools place outputs under the selected
game's build directory when the game is outside the kit.
