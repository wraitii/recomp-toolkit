# Testing

Use a Python environment installed from `requirements-dev.txt` and invoke
`tools/build.py` with that environment's Python; CMake uses the same interpreter
for bundle finishing. Its pinned requirements include `pefile`, Pillow and
resvg-py for app icons. An executable link alone does not establish a finished bundle.

| Command | Checks | Game needed |
| --- | --- | --- |
| `tools/test.py --native` | Native CTest suites (runtime, DirectX, Metal, controls) | `game`-labelled suites only |
| `tools/test.py --compile-only` | Native test binaries build | No |
| `tools/test.py --translator` | Synthetic assembly cases translated and compared with Unicorn | No |
| `tools/test.py --mods` | Mod loader, hooks and replay against the translated archive | Yes |
| `tools/test.py --d3d8-wgpu test\|probe` | Rust D3D8 renderer tests or offscreen probe | Yes, plus a GPU |
| `tools/build.py --renderer-consumer build\|run` | Build, or build and run, the independent D3D9 CPU buffer consumer | No |
| `tools/build.py --renderer-consumer gpu-build\|gpu-run` | Build, or build and run, the independent D3D9 offscreen GPU consumer | No game; GPU to run |
| `tools/build.py --renderer-consumer probe-build` | Compile the headless renderer probe without windowing; no execution | No |
| `tools/build.py --stub --no-pcode --target d3d9-guest-tests` | Build the headless D3D9 guest COM stub; run `d3d9_guest_tests` from the build root's `stub/recomp/` directory | No game; GPU to run |
| `tools/build.py --stub --no-pcode --target compositor-tests` | Build existing compositor, host, GPU, DirectX, layout and guest graphics checks | No game; GPU for pixel checks |
| `tools/build.py --compositor-consumer build\|run --compositor-frames 8` | Independent Rust compositor/surface host; run exits after the requested frames | No game; window and GPU to run |
| `tools/format.py` | Native code formatting | No |

CTest labels: `nogame`, `game`, `gpu`, `device`, `mods`. Run a subset with
`ctest --test-dir <build root>/cmake/macos -L nogame` or `-R dx_tests`.
The D3D9 CPU consumer disables the renderer's windowing and probe features;
the renderer still builds its CXX bridge.
The compositor target builds without running checks. From `stub/recomp/`, run
`compositor_tests`, `host_tests --presenter-only`, `host_tests --presenter-gpu`
and `headless_tests <capture-directory>`. `gpu_wgpu_tests` covers the backend,
`layout_tests` checks resource paths, and the DirectX/D3D8/D3D9 binaries cover
their retained APIs. CPU layout, fake presenter and headless checks need no GPU;
pixel checks use Rust/wgpu. Retired D3D11 checks are intentionally removed.

Push and pull-request CI requires portable tooling and macOS stub builds, including
the game-free native and GPU suites. Native Windows and Linux jobs are optional;
their failures do not fail the workflow. Android, iOS and Windows cross-compilation
remain in the manually dispatched `Experimental platform checks` workflow.

`d3d8_surface_tests` (`gpu`) exercises the backbuffer descriptor, texture
levels and staging, vtable calling convention, identity, bounds and
device/factory ownership through guest COM dispatch. Its devices are
constructed state fixtures, with offscreen GPU devices for render-target binding;
it does not demonstrate game rendering. It uses Rust CPU storage and mip layouts
through CXX, including
live locked resources across generation reset.
`d3d9_guest_tests` (`gpu`) creates a headless guest Win32 window, calls the D3D9
factory import and exercises device/resource methods through guest COM vtables.
It checks offscreen drawing and textured pixel readback, nested buffer locks,
retained-pointer writes, allocation failure/retry, descriptor store hooks,
texture/surface aliases and live locks across heap reset. Reset rejection
preserves a managed texture; successful Reset retains its lock, and writes after
Reset reach a GPU sample. SDL surface presentation and game rendering remain
outside its coverage. The stub uses the no-p-code test configuration.
`d3d7_mipmap_tests` (`gpu`) follows DirectDraw implicit mip attachments and
per-level locks through D3D7 texture binding and real GPU readback. It checks
capability reporting, point/linear mip filtering, fractional LOD bias,
MIPFILTER=NONE and updates to a child without rebinding the root. `dx_tests`
covers rectangular mip dimensions/counts, attachment traversal, independent
storage, one-level requests and partial allocation rollback without a GPU.
`--unsupported-lock` and
`--unsupported-texture` are child probes expected to abort with named
diagnostics. When running `runtime_tests --startup-contracts` directly, set
`RECOMP_PYTHON` to the absolute Python interpreter containing `pefile`.

`tools/recomp/diff/run.py` compares translated game functions with Unicorn, and
with `--mode paired` compares the production specialization-enabled and forced
ordinary variants on each shared input; see `AGENTS.md`. The function corpus
(`tools/build.py --function-corpus`) compares the
eager and SSA translations of real game functions; see its
[README](../tools/recomp/corpus/README.md).

Diff reports include separate ordinary/original, specialized/original and
ordinary/specialized findings, with the first failing input and iteration.
Every run keeps its linked harness beside `report.json` and `run.json` in a new
directory; `--no-build` copies the prebuilt library under the build lock.
`run.json` records executable, harness, config, code-map and fact hashes, the
translation fingerprint, selected functions and execution limits. Baseline
snapshots are read and written only through explicit paths; existing report or
baseline destinations cannot be overwritten. Writable image sections,
BSS/scratch/TEB fills and the full zeroed stack are reset before every execution.

## Media probes

`RECOMP_TEST_AUDIO=/path/to/track.ogg` and `RECOMP_TEST_AVI=/path/to/movie.avi`
exercise the Vorbis and AVI/Indeo decoders in the native suites.

`--record-inputs DIR` saves every input, including discarded inputs and complete
scratch/BSS/TEB fills, in a fresh directory. `--replay-inputs DIR` checks
executable, selected functions and input settings before replay. Original
memory is reset independently for each mode.

The diff oracle excludes architecturally undefined output flags and discards
inputs that read empty x87 slots. Unicorn does not reliably report the x87 stack
fault bit, so the harness also checks operand tags before each instruction.
Ordinary/specialized comparisons still include every tracked flag. Accepted
binary32 exponent-range and extended-precision differences remain visible in
original-byte comparisons; see [historical IR tradeoffs](../tools/recomp/Deprecated.md#scalar-x87).

`--mode paired` compares the production specialization-enabled and forced
ordinary variants within a run; `pair` contains those discrepancies and `oracle`
remains separate. `--fail-on pair` requires paired mode and ignores the non-clean
oracle baseline. A saved library's coverage must be supplied with its build
evidence; its name alone does not prove a private route was executed. Existing
x87 slot/status, fault-discard and gameplay coverage limits still apply.

## Graphics upstream verification

Use the configured Python environment with `tools/requirements-library.txt`
installed. From the kit root, the implementation checks are:

```sh
PYTHON=build/python/bin/python
$PYTHON tools/build.py --stub --target compositor-tests --jobs 8
ctest --test-dir build/cmake/macos-stub --output-on-failure -R '^(compositor_tests|host_tests|headless_tests|gpu_wgpu_tests|gpu_fake_tests|dx_tests|layout_tests|d3d8_surface_tests|d3d8_render_tests|d3d9_guest_tests|d3d7_mipmap_tests)$'
$PYTHON tools/build.py --renderer-consumer run
$PYTHON tools/build.py --renderer-consumer gpu-run
$PYTHON tools/build.py --compositor-consumer build
```

GPU suites need access to the native adapter. The compositor window can be
exercised with `--compositor-consumer run --compositor-frames 8`. Other operating
systems use their matching CMake build directory; macOS/Metal is the initial
qualification target. Existing no-game suites do not qualify gameplay or pacing.

Build the selected game with `tools/build.py --game <id> --target app`. For the
runtime comparison, record both revisions, game executable hash, save, camera,
graphics profile, window size, adapter and run duration. Use the same save and
input workload on both builds. Exercise intro/settings and sustained gameplay,
F10/F11, input mapping, capture, resize/minimize/restore and shutdown with work in
flight. Include capture callbacks that query the presenter or request shutdown,
and supported reset/loss paths. Compare CPU/frame time, pacing, copies/readbacks
and retained memory. CMP measures successful GPU completions, not display latency.

For the portable macOS toolkit, use a standalone arm64 Python installation:

```sh
$PYTHON tools/build.py --target toolkit --package-toolkit --toolkit-python-root /absolute/path/to/standalone-python
```

Build a game from the resulting toolkit on a machine without the development
Cargo cache, verify both staged graphics archives and their headers are present,
and check that manifest source/artifact hashes match the bundle. Commercial game
files, profiles, logs and generated build files must be absent. Full packaging
and runtime qualification are the final user verification step.
