# Translator and low-level tools

The contributor entry points are [setup.py](../setup.py), [build.py](../build.py)
and [test.py](../test.py). They check prerequisites and select the commands below.
Use those wrappers for an ordinary first build.

| Tool | Responsibility |
| --- | --- |
| `runtime/x86.h` | Register/flag/x87 state and instruction semantics used by generated code |
| `buildlock.sh`, `buildlock.py` | The shared process lock over build/recomp, as a shell entry and a Python module |
| `build_core.py`, `finish_bundle.py`, `snapshot_gen.py` | Install core mods reproducibly, finish the app bundle, copy one consistent generation of gen/ |
| `texture_pack.py`, `package_texture_pack.py` | Prepare and package optional local replacement textures |
| `performance_run.py` | Bounded presentation measurements and diagnostics |
| `diff/`, `tests/` | Unicorn differential harness (`diff/run.py`) and the native host (`tests/harness.c`) it and the entry-thunk fixture build on |

Generated functions preserve addresses, symbol names and instruction comments.
They are a mechanical translation, so register operations are expected. Add
meaningful names in reviewed symbol metadata; do not hand-edit generated chunks or
publish them.

A normal translation needs the exact executable prepared by setup and the game's code map.
`tools/build.py` publishes the generated directory and `symbols.json` by rename
under the lock, then CMake rebuilds `librecomp_gen.a` from it; a reader under the
same lock never sees half a generation.

Game-specific smoke scripts and original-game comparisons belong in the game
repository. See [Testing](../../docs/testing.md) for the kit's generic suites.

The legacy Python translator, eager/SSA corpus, standalone bootstrap map generator,
type inference and x87 audit producers have been retired. Existing facts and native
reference assets remain; Rust analysis produces some facts but does not replace every
historical research workflow. Create initial maps with the shared Ghidra exporter.
