# Railroad Tycoon 3

Native port of **Railroad Tycoon 3 (2003)**. Fully playable in single player
with good performance. Networking is currently unsupported.

## Required copy

Use the GOG release with official patch 1.05 and no unofficial user patch.
The installation must contain `RT3.exe` and its game data. Disc editions are
unverified.

## Build from source

The reference is the GOG `RT3.exe` (32-bit x86, MSVC 7-era C++), identified in
[game.toml](game.toml). This directory is the configuration root
(`--game railroad-tycoon-3`).

From the game repository root (the one containing `recomp-kit/`); prerequisites are in
[CONTRIBUTING](../../CONTRIBUTING.md#prerequisites):

```sh
git submodule update --init --recursive recomp-kit
python3 -m venv tools/.venv
tools/.venv/bin/python -m pip install -r recomp-kit/requirements-dev.txt
RECOMP_BUILD_ROOT=$PWD/build tools/.venv/bin/python recomp-kit/tools/setup.py \
  --game railroad-tycoon-3 --install "/absolute/path/to/your/installed/game"
tools/.venv/bin/python tools/build.py --regenerate --target app --jobs 8
open build/RailroadTycoon3Recomp.app
```

Setup links the installation at `build/original` without changing its files.

For subsequent builds:

```sh
tools/.venv/bin/python tools/build.py --target app --jobs 8
```

Add `--regenerate` after changing translation inputs: the code map, translator
or translation settings. The image still contains MMX/3DNow (`femms`, `pf*`) and
scalar-SSE instructions the translator cannot model. They translate to a trap at
their own address, and remain fidelity gaps if reached (see Status below).
`--regenerate --target gen` generates translated C without configuring or
building the app.

Translation uses the kit defaults: SSA, relaxed
fault-state publication (`fault_state = "relaxed"`) and MSVC x87 call convention
(`msvc_x87_convention = true`). `DIVERGENCE(original):` relaxed publication can
leave diagnostic CPU/x87 state stale at a fault; the MSVC convention elides
popped x87 residue at calls and returns. For conservative comparison, set
`fault_state = "exact"` and `msvc_x87_convention = false`,
then regenerate.

`[game] store_hooks = false` removes per-store watchpoint and DirectDraw dirty-range
checks. DirectDraw Unlock compares the whole locked surface against its saved
contents, and `RECOMP_WATCH` diagnostics are unavailable. This trades cheaper
guest stores for more Unlock scanning; frequent small writes to large locked
surfaces can favor hooks. The hooks-disabled app rebuilds and all 31 native
suites pass. An approximate in-game profile comparison confirms the store-hook
samples are gone, but scene differences leave the net performance change
inconclusive.
Set `store_hooks = true` and rebuild to restore dirty-range tracking and watchpoints.

`[game] call_hooks = false` builds without mod call hooks: calls no longer
republish register fields for hooks or reread kept fields afterwards, mods that
install hooks are refused, and `RECOMP_TRACE_CALLS`/`RECOMP_TRACE_LINEAR` are
ignored with a diagnostic. Profiling and frame watch still take the generic call
routes. Set `call_hooks = true` and regenerate to restore them.

## Status

Single-player gameplay is fully playable and performant. Networking is unsupported.

The code map is exported and validated: 6,039 functions and 510,070
instructions, decoded from the real executable by `tools/build.py --regenerate`
and in agreement with Ghidra's boundaries. The game boots, loads a map and
renders the 3D world (terrain colour and detail, UI, DXT textures).

Unsupported SSE/XMM and 3DNow instructions emit named traps. The previous
52 `skip_functions` entries are unnecessary with the current kit; all mapped
functions are translated. The kit now lowers `movups xmm3, xmmword ptr [ecx]` at the former first stop,
`00581699`; `mulps xmm2, xmm3` at `005816a8` still traps. The runtime reports a CPUID
without MMX/SSE/3DNow, so the guest selects the x87 math tier. Full vector
lowering remains necessary before advertising those features.

The v3 map adds Ghidra's jump targets, referenced interior entries and
non-returning functions/calls. Its migration exposed stale function bodies,
misdecoded instructions and four missing callable helpers, repaired against
the original executable. See Ghidra cleanup notes (`docs/ghidra-cleanup.md` in the game repository).
Generation emits 5,859 SSA bodies and 180 decoded bodies. It reports 128
unmodelled instructions, while the generated bodies contain 3,443 distinct
instruction trap sites including vector fallbacks not counted by that report.
Computed jumps without recorded targets use runtime dispatch; missing entries
use the kit's [p-code fallback](../../docs/architecture.md#discovery-and-code-a-build-does-not-carry),
which stops explicitly on unsupported semantics. `--no-pcode` omits this fallback
at build time; `RECOMP_INTERP=off` disables it at runtime.
Advertising SSE support requires auditing the remaining lowering gaps first.

Memory forwarding now shares value phis across joins and loop backedges while
retaining guest stores and unknown-write/call barriers. Across the 85 affected
functions (93 compiled variants), ARM64 bodies have 121 fewer load instructions
and 28 more store instructions. Total size grows from 202,176 to 202,312 bytes,
including unique outlined helpers; 32 bodies shrink, 48 stay the same size and
13 grow. These are static counts, not runtime speed measurements. Forwarding
can expose loop bounds to Clang and increase unrolling and spills: `004f8770`
loses 14 loads, seven stores and 72 body bytes, while `00454d70` gains seven
loads, 21 stores and 320 body bytes. No profitability heuristic is applied.

Validation: the existing 25 translator cases passed at 200 inputs each. Paired
original-byte comparisons covered the 85 changed functions: 52 passed after
two timeout rechecks, 30 had no comparable return, and three x87 discrepancies
also reproduced without this change; no ordinary/specialized pair differed.
The windowed app builds and was not launched for this change.

Host overhead: the renderer uploads only each draw's requested vertex interval,
including packed software-lighting output, and the guest-store watch/dirty gate
is forced inline. Vertex/index buffers retain guest backing across locks, and
unchanged render-target texture binds retain queued draws. Guest COM lifetime
checks and Metal readback cover these paths. Compact unlit triangle lists use
GPU indices, including vertex shaders with declaration-derived layouts and
padded strides. Fixed-function lighting for 0x112/0x152 runs in the GPU vertex
shader for both indexed and nonindexed draws. Lit triangle lists pack distinct
referenced vertices and remap GPU indices, skipping unused sparse gaps.
`RECOMP_D3D8_CPU_LIGHTING=1` forces the CPU comparison path; unsafe attenuation,
nonfinite state/positional inputs and `ProcessVertices` retain CPU processing.
`DIVERGENCE(original):` GPU lighting rounding and `pow` may differ from the CPU
reference and original D3D8 hardware; synthetic Metal comparisons allow up to
two 8-bit channel levels per pixel.
Sparse unlit, strip/fan and diagnostic draws retain expansion
(`RECOMP_D3D8_EXPAND_INDICES=1` forces it for comparisons). CPU byte-equivalence
and Metal readback cover both lit layouts, both index widths, offsets, repeated
indices and queued snapshots. A fresh in-game profile is needed to quantify the
latest performance change. Validation: 161 Rust tests and the Metal headless
probe pass; the app rebuilds. Native suites pass 31/31 with Metal and game-directory fixture access. These checks
establish synthetic renderer equivalence, not original-D3D8 gameplay equivalence.

Known gaps and diagnostics:

- Samply profiles include `frame` spans at each guest window-present boundary,
  covering D3D8 native texture handoff and CPU readback. Markers enable
  automatically when launched under samply; `RECOMP_PROFILE_MARKERS=1` also
  writes `marker-<pid>.txt` in the system temporary directory. Spans measure
  time between guest presents, including pacing, rather than GPU execution time.
- `RECOMP_D3D8_DUMP_TEXTURES=<dir>` writes bound texture levels as PNG
  (`RECOMP_D3D8_DUMP_TEXTURE_LEVEL=<n>` limits to one level);
  `RECOMP_D3D8_TRACE_DRAWS=<n>` (`RECOMP_D3D8_TRACE_DRAWS_START=<n>` skips that
  many draws first; `RECOMP_D3D8_TRACE_STAGE1=1` restricts to two-stage world
  draws) and `RECOMP_D3D8_TRACE_PROCESS_VERTICES=<n>` dump draw and
  `ProcessVertices` state; `RECOMP_TRACE_CALLS` traces guest function entries.
  `RECOMP_D3D8_TRACE_SHADERS=1` reports shader creation handles and token counts.
  `RECOMP_D3D8_TRACE_SHADER_DRAWS=<n>` captures four shader bindings, constants
  and sample vertices; `RECOMP_D3D8_TRACE_SHADER_STAGE2=1` selects shaders that
  sample stage 2 (the water pass), and `RECOMP_D3D8_TRACE_SHADER_DIR=<dir>`
  captures GPU-rendered inputs as PNGs.
- The kit implements D3D8 vs.1.1/ps.1.1 creation, binding, deletion, queries,
  declarations, constants and four-stage sampling with `texbem` bump matrices.
  Signed `V8U8` bump textures retain their two-byte guest layout and signed
  GPU samples across mip levels, supporting the ocean bump maps produced by
  `FUN_00542550` (`0x005425ea` stores format `0x3c`). Shader caps enable the
  engine's programmable path. Startup creates all five shipped shaders
  successfully; synthetic Metal readback tests cover constants and dependent
  sampling. A guest COM/GPU regression verifies that rendering to a texture
  surface and sampling its parent texture share the same GPU image. A manual
  in-game check reports the dark water/reflection issue resolved; original-D3D8
  visual equivalence remains unverified. See [shader coverage](../../graphics/d3d8-wgpu/SHADERS.md).
- `IDirect3DTexture8::SetLOD` is not applied to the mip clamp, and render-target
  textures bind level 0 only.
- The terrain's colour map is extracted by the game from the `.gmp` to a loose
  `Maps/<name>_A.tga`, then found through the directory scan in
  `GrafxResMgr::ScanAndRegisterFiles`. That scan depends on `FindFirstFile`
  returning `.` and `..` first, which the kit's kernel32 shim now does.


## Code map

The GOG `RT3.exe` identified by [game.toml](game.toml) is the behavioral reference. The packed [code map](metadata/) contains 6,039 functions across 6,760 spans and 510,070 instruction boundaries, without executable bytes, instruction text or analysis names. The private executable supplies those bytes during translation.

Ghidra research, export scripts and engine notes remain in the game repository. To refresh the map, export `recomp-code-map-v3-export` from the matching executable into a fresh directory, then run:

```sh
python tools/recomp/code_map.py --pack /path/to/v3-export --exe /path/to/RT3.exe --out games/railroad-tycoon-3/metadata
```

The map pins the executable SHA-256 and preserves disconnected function bodies and exceptional instruction boundaries. It also records Ghidra jump targets, referenced interior entries and non-returning functions/calls. Verify every boundary repair against original bytes before exporting.

Unsupported vector instructions emit named traps at their original addresses; the previous 52 `skip_functions` entries are no longer needed. The kit now lowers `movups xmm3, xmmword ptr [ecx]` at the former first stop, `0x00581699`; `mulps xmm2, xmm3` at `0x005816a8` still traps. Remaining SSE/3DNow gaps still trap; CPUID feature advertisement is unchanged.

`DIVERGENCE(original):` RT3 assumes `private-stack` and `stack-pointer-bases`:
unescaped frame bytes are reached only through ESP/EBP, and escaped frame
pointers bound access from below unless callee evidence widens that range.
Remove a name from `[translate] assume` to drop its premise; [translator details](../../docs/translator.md)
describe guarded calls, range exposure and conservative fallbacks.

The checked-return pilot selects `0x005a10d0` (through `0x005a1144`), the x87
integer-conversion helper. Original bytes show a single-entry leaf with aligned
stack scratch, path-dependent ECX/flags and one RET; callers include `0x0042c1b0`
and `0x0042ce00`. Eligible SSA direct calls pass their decoded continuation and
skip general return lookup on a match. Stack writes, x87 effects and all existing
state publication remain translated. Decoded callers, indirect calls, callbacks,
hooks and profiling retain the ordinary entry. Set `checked_returns = []` under
`[translate]`, also disable `private_leaf_contracts`, and regenerate to disable checked calls. Savings across the conversion call's
state transfer are piloted separately below; scratch memory remains translated.

The experimental integer-state pilot selects the reviewed callers
`0x0042c1b0` and `0x0042ce00` through
`state_contract_callers`. Their conversion edges require EAX, ESP and EBP on
normal continuation, with AF retained at sites where it remains
observable; EIP and all existing x87 state remain published. Private
entries omit other integer return stores and use byte-derived leaf input
contracts. Full changed state is recovered on an unexpected RET, and observation
boundaries retain the ordinary path. Set `state_contract_callers = []` and
disable `private_leaf_contracts` and regenerate to return to ordinary calls.
Automatic discovery extends these contracts to returning leaves with at least
two direct call sites and at most 64 original instructions per site, subject to
closed single-entry, replacement/SEH and emitter checks. One shared output layout
is emitted per callee; the report records automatic selections and rejections.
Non-leaf specialization and x87 value-transfer contracts remain future work.

Validation compared both private output sets and their unexpected-return
paths against the ordinary entry over 1,860 rounding cases each, with no new
differences. The ordinary entry still has the baseline's 96 original-byte
rounding discrepancies. Random-input diff coverage reached valid returns in
`0x005a10d0` and `0x0042c1b0`, including hook-enabled ordinary fallback, but not
`0x0042ce00`. Cleanup removes fallback parity reductions in the private leaf
assembly and reduces caller stack frames. Eligibility and checkpoint preparation
share one policy call; private entry thunks tail-call their bodies. Workload
measurement uses host sampling with `RECOMP_PROFILE` unset or `0`, since the
built-in guest profiler selects ordinary fallback. No workload gain is established.

Static control-word analysis proves the conversion leaf preserves its incoming
mode. Private preparation now requires `(CW & 0x0f00) == 0`, and each private
body emits only the fast arithmetic; other modes reuse the ordinary checked
entry. `x87_cw_clone = false` keeps x87 calls on general ordinary entries. In the linked
ARM64 diff harness, the two private bodies shrink from 4,680 to 1,120 bytes;
preparation grows by 12 bytes. The existing rounding fixture, routed through
preparation for private calls, matches the captured ordinary implementation in
9,300 cases across both output masks, fallback modes and unexpected returns,
including full x87 state and scratch memory. The ordinary original-byte fixture
retains its 96 inherited discrepancies; the random-input diff comparison passes
200 valid returns without discarded inputs.

The selected callers now share one private body with their union output mask
`0431` (`EAX`, `ESP`, `EBP`, `AF`), while keeping per-edge liveness and reloads.
Its linked ARM64 size is 564 bytes, removing the redundant 556-byte copy.
The shared layout passes 9,300 comparisons against the captured ordinary body,
including fallback modes, full x87 state, scratch memory and unexpected RETs.
Original-byte checks pass for the helper and one caller; the other caller is
unreached under random inputs. Ordinary entry and preparation policies are kept.

The code-map inventory has 58 functions containing 78 control-word writer sites.
Their direct-call descendants include 81 of 1,565 x87-using functions. This
narrows the exceptional-mode review; it does not establish a default at the
remaining entries. The generated contract report includes writer sites, exposure
closures, unknown boundaries and x87-only preservation coverage: 517 of 1,565
functions prove intrinsic precision/rounding preservation. Offline analysis with
selected boundary contracts raises that to 525; incoming constants remain separate. Local stack provenance
proves the original `__ftol` save/truncate/restore sequence; shared globals,
indirect calls and startup-to-frame propagation remain conservative.

The FEMMS regeneration changes 62 functions. Random-input original-byte checks
reach no comparable returns in those functions; instruction checks cover tag
clearing and control-word preservation. TOP follows AMD's documented reset,
where Unicorn's Athlon model differs; see [historical IR notes](../../tools/recomp/Deprecated.md#scalar-x87).

Private continuation layouts retain modified registers absent from the caller's
SSA and values observed by later calls, even when no additional publication is
needed. The original-byte regression suite exercises eight cases, including
`005a76d8`'s EAX exponent result before CRT math calls and `0052ecd0`'s EDX
constructor result escaping through its caller. Run after rebuilding:

```sh
python tools/recomp/diff/run.py --game railroad-tycoon-3 --suite games/railroad-tycoon-3/validation/private-call-state.json
```

Each run writes a fresh report directory holding its own linked harness;
`--mode paired` runs the production specialization-enabled and forced ordinary
routes on the same inputs and compares both with the original bytes. `--changed`
needs an explicit `--baseline FILE`, recorded with `--save-baseline FILE`;
neither is implicit. Before a translator change: `--all --save-baseline
before.json`; after the change and `--regenerate`: `--changed --baseline
before.json`. Run the pair check on callers with specialized edges, such
as `0x0042c1b0` and `0x0042ce00`. The harness cannot observe whether one call
site took a private route, and compares only the active ST0/TOP, so pair equality
is necessary but not per-call-site coverage.

The tag-aware checker also reports empty/active ST0 disagreements at
`005a13d0`, `005a3040` and `005a3e90` in both modes. These wrappers consume an
x87 argument, while the current random fixture starts with an empty x87 stack.
They remain oracle failures under the default policy; pair-only diagnosis can
separate them from specialization differences. Valid x87 argument fixtures and
full-stack/status comparisons remain validation gaps.

Selected production facts use import PC/RC preservation and the explicit,
verified cargo-entry premise in `game.toml`. `x87-boundaries.json` records the
byte-backed native/import boundaries; `x87-facts.json` covers 479 instruction
sites in 10 functions. It removes five ordinary arithmetic copies and six
private call-site CW checks while retaining other preparation and fallback
conditions. Exploratory roots and allocator/math assumptions remain offline.
Re-export facts with `tools/recomp/x87_audit.py propagation --facts-out` after
changing an analysis input or bumping the analysis producer version. Specialized calls transfer state through `X86`
(CPU fields); the value/result ABI and LLVM backend trials were rejected.
