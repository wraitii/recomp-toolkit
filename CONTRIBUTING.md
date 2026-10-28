# Contributing

Use the [desktop app](README.md#get-started) to install supported games. This
guide covers source builds, toolkit development and new game ports. Read
[AGENTS.md](AGENTS.md) and the game's own instructions before making changes.

## Prerequisites

Apple silicon macOS is the primary target. See [Platforms](README.md#platforms)
for Intel Mac, Windows and Linux support.

- Python 3.9+ with a virtualenv holding `requirements-dev.txt` (CMake and Ninja
  come from it). On Windows use `.venv/Scripts/python.exe` below.
- macOS: Xcode Command Line Tools. Linux: clang, lld, make and the SDL headers
  listed in `.github/workflows/checks.yml`. Windows: LLVM clang in a Visual
  Studio developer prompt. Builds also require Flex and Bison 3.0 or newer.
- Your own installation of the game. `game.toml` pins the executable's SHA-256.

From the kit root:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
```

## Prepare a game

```sh
.venv/bin/python tools/setup.py --game <id> --install "/path/to/installed game"
```

Setup checks the executable hash and links the installation at
`<build root>/original`; it does not copy or change your game files. Translation
needs the game's code map.

Tools that select a game accept `--game <id>`, or `--game-dir <path>` for a game
kept elsewhere. Outputs go to `build/<id>/` for a kit game and `<game-dir>/build`
otherwise; `--build-root` or `RECOMP_BUILD_ROOT` overrides both.

Games also have their own repositories with this kit as a submodule. Follow the
matching [game README](README.md#games) when building from one of those checkouts;
its `tools/build.py` wrapper selects the game and build directory for you.

## Build

```sh
.venv/bin/python tools/build.py --game <id> --target app
.venv/bin/python tools/build.py --game <id> --regenerate --target app
```

Use `--regenerate` after changing the code map, translation settings or translator
sources. Runtime, host and renderer changes need only the appropriate target
build.

Other targets: `smoke` (offscreen scripted host), `headless`, `plugins`, `ios`,
`android`. `--preset` and `--config Debug` select the CMake configuration, whose
tree lives in `<build root>/cmake/<preset>`. `RECOMP_PROFILE_DIR` selects a
separate player profile. `-DRECOMP_VIDEO=OFF` at configure time builds without
FFmpeg. `--stub` builds a host without game code for toolkit development.

### Desktop app

Building the desktop shell requires Rust and Cargo in addition to the tools above.

```sh
.venv/bin/python tools/build.py --target toolkit
```

On arm64 macOS, add `--package-toolkit --toolkit-python-root /path/to/python`
with a standalone Python installation to bundle the source kit, compiled Rust
translator, Python orchestration tools, CMake, Ninja and native dependencies.
The packaged app needs Apple command line tools but no Rust toolchain.
The [desktop workflow](.github/workflows/toolkit.yml) contains the packaging recipe.

Successful main builds update the `latest-nightly` prerelease after Developer ID
signing and notarization in the [distribution workflow](.github/workflows/distribute.yml).
Its repository secrets are `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`,
`MACOS_CERTIFICATE_BASE64` (the exported signing identity and private key as a
base64-encoded P12), `MACOS_CERTIFICATE_PASSWORD` and `MACOS_SIGNING_IDENTITY`.
Pending Apple submissions are preserved for 90 days and checked every 30 minutes;
dispatch `distribute.yml` manually to check sooner. Signing covers bundled
executables, Python extensions and libraries before the outer app.

## Add a game

The translator targets 32-bit x86 Windows executables. To add a game, identify
its executable, generate a code map, then build and work through any
missing instruction, OS, graphics or audio support.

### Identify the release

Choose a release and record the executable's SHA-256, image base, entry point
and data layout in `game.toml`. Add the identities of any DLLs that also need
translation.

A game directory contains:

```text
games/<id>/
  game.toml       identity, translator inputs, hooks and host settings
  metadata/       the code map: function spans, no instruction bytes
  native/         optional native replacements for translated functions
```

Start from an existing game's configuration, replacing its identity, addresses
and settings with those from your game. For a port outside the kit, pass its
directory with `--game-dir`.

### Generate a code map

For an initial map, use the standalone generator from the kit root:

```sh
.venv/bin/python tools/recomp/bootstrap_map.py --exe /path/to/GAME.EXE \
  --out games/<id>/metadata --report /tmp/<id>-map.json
```

Set `[translate] code_map = "metadata"` in `game.toml`, then run setup. Build
these provisional maps with `--regenerate`.
The generator recovers code from the executable using entry points, relocations,
pointer tables and an instruction sweep. Its report records gaps and conflicts.

We use [Ghidra 12.1.3](https://github.com/NationalSecurityAgency/ghidra/releases/tag/Ghidra_12.1.3_build)
with a matching JDK to produce richer maps with reviewed function boundaries,
jump tables, interior entries and non-returning calls. Ghidra is optional.
Adapt a game's export script from its research repository; the
[Ghost Recon workflow](games/ghost-recon/DEVELOPMENT.md#code-map) gives an example.
Pack the resulting `recomp-code-map-v3-export` directory against the executable:

```sh
.venv/bin/python tools/recomp/code_map.py --pack EXPORT_DIR --exe GAME.EXE --out games/<id>/metadata
```

Maps contain addresses and boundary metadata, without executable bytes or
analysis names. After correcting a map, regenerate the translation. Auxiliary
modules have their own maps and configuration.

### Bring up the game

Run setup and regenerate an app build using the commands above. Follow the
reported diagnostics to add missing support, using the original disassembly and
callers to understand the required behavior. Unsupported instructions generate
named traps that remain fatal if reached.

Keep reusable support in the translator, runtime or platform adapters. Game
addresses and settings go in `game.toml`; native replacements go in the game's
`native/` directory. The [architecture](docs/architecture.md) explains guest
memory and runtime boundaries, and the [code guide](docs/code-guide.md) maps
behavior to source. [AGENTS.md](AGENTS.md) covers fidelity requirements and the
workflow for native replacements.

### Add desktop setup

Add `[setup]` version and source guidance so the app can explain which copy to
choose. Configure the installation layout, required modules and any installer
recipes for that release. In the game's README, describe preparation steps,
working features and known bugs.

## Observe indirect calls

Developer builds include OBSERVE, which records indirect call and jump targets.
Run the game's executable with an output path:

```sh
RECOMP_OBSERVE=/tmp/edges.tsv /path/to/Game.app/Contents/MacOS/Game
```

Quit normally to write the report. Each row contains `site target kind count desc`:
the guest addresses of the transfer and target, how it resolved, its hit count
and an import description where available. Use the report to investigate dispatch
and feed observed targets into [analysis facts](docs/translator.md#analysis-facts) with
`tools/build.py --game <id> --produce-analysis-facts /tmp/facts.json
--analysis-observation /tmp/edges.tsv`.

Observation adds overhead when enabled. Leave `RECOMP_OBSERVE` unset for performance
measurements. To compile it out entirely, build with `--no-observe`
(`RECOMP_OBSERVE=OFF` in CMake). Games built through the desktop app always use
this option. A regular source build includes observation again.

## Check translation

`tools/recomp/diff/run.py --game <id>` runs translated functions and the
original bytes side by side and compares the results; the function corpus
(`tools/build.py --function-corpus`) checks them against each other. See
[AGENTS.md](AGENTS.md) for the translator-change workflow and
[Testing](docs/testing.md) for harness details.
