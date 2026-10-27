# d3d8-wgpu

Isolated Rust + wgpu prototype of a **D3D8-semantics renderer**. It exists to
answer one question independently of the game: can a small, reusable D3D8 host
layer (state, resources, fixed-function emulation,
rendering) be built on wgpu — and specifically on Metal on ARM64 macOS, where
the DXVK/MoltenVK experiment failed?

The crate exports a host-only C ABI used by recomp-kit. It contains no guest CPU
or COM types and no game addresses. C++ owns guest dispatch, interface identity,
reference counts and guest staging allocations; Rust owns CPU resource bytes,
mip layout calculations, indexed-draw preparation, render state and GPU work.
The native resource tests link the same Rust storage without creating a GPU.
D3D9 decoding/admission and specialized WGSL emission live in `src/shader/`;
`src/render/` owns immutable vertex layouts and prepared pipeline descriptors.
D3D9 retains state interpretation, caches, submission and validation ownership.
Owned stage recipes retain decoded programs and explicit variant/output settings,
or captured fixed-function source. Complete pipeline recipes add resource/vertex
interfaces, entry points and raster/output descriptors; prepared GPU handles stay
separate. Draw parameters take explicit values and constant-bank slices, copied
into existing batch storage before later state changes.

- Lives in recomp-kit and is built by `dx/CMakeLists.txt` when `RECOMP_D3D8_WGPU` is on
  (or a game ships its own `graphics/d3d8-wgpu`). Game-specific usage inventories and
  execution evidence belong in the game repository, alongside its README and engine notes.
- Some draw paths cover only what a game has needed so far (for example the lit
  `0x112`/`0x152` FVFs); anything else fails by name rather than approximating.
- `reference/` is gitignored local material (Wine's D3D8 headers); the pinned copy the
  generators use is `third_party/wine-d3d8/`.

## Status

Fixed-function DotProduct3 evaluates signed RGB components, saturates the
result, and replicates a color dot product to alpha. Alpha-only DotProduct3
uses the RGB components of its selected arguments. A8 texture uploads consume
one alpha byte per texel and sample as `(0, 0, 0, alpha)`; A8 render targets
remain unsupported. GPU readback tests cover both paths.

The bridge uses opaque Rust storage for texture levels and vertex/index buffers.
Compact triangle lists queue GPU indices, including programmable shaders with
stream-0 declaration layouts and padded strides. Fixed-function lit lists pack
only distinct referenced vertices for GPU lighting or CPU fallback and remap indices;
unused sparse gaps are never evaluated. Checked arithmetic validates actual
indices and base/start offsets rather than upload hints. Sparse unlit spans,
strips/fans and diagnostic draws retain expansion; `RECOMP_D3D8_EXPAND_INDICES=1`
forces it for comparisons. Queued vertex/index bytes and constants are immutable
snapshots. Shaders and supported render pipelines are cached per device.
Pixel float constant storage holds c0–c223 for uploads, readback and state blocks,
independently of shader-model-specific bytecode limits, matching
[DXVK's D3D9 constant storage](https://github.com/doitsujin/dxvk/blob/master/src/d3d9/d3d9_device.h).
Buffer locks accept DISCARD/NOOVERWRITE on static buffers as ordinary writes,
matching [Wine's D3D9 compatibility behavior](https://github.com/wine-mirror/wine/blob/master/dlls/wined3d/device.c).
DrawIndexedPrimitiveUP supports triangle lists and fans with 16/32-bit guest
indices and a single vertex stream. It checks guest spans before borrowing,
queues immutable snapshots, and clears stream zero and index bindings.

Programmable declarations support sixteen vertex streams. Multi-stream draws
reuse D3D9 index ordering and checked byte fetches to pack only referenced
attributes into immutable vertex snapshots, preserving register types and bits.
Device caps advertise `D3DDEVCAPS2_STREAMOFFSET`, matching the supported
per-stream byte offsets for suballocated vertex buffers.

Clear accepts host copies of guest D3DRECT arrays, clips each rectangle to the
current viewport, and preserves color/depth/stencil outside the selected regions.
An empty array clears the viewport. The bridge validates guest spans and copies
unaligned records before calling Rust; rectangle memory is consumed before return.
SetViewport rejects rectangles outside the current logical render target with
D3DERR_INVALIDCALL and preserves the previous viewport. Empty viewports clear no pixels.

CreateDevice and Reset accept lockable 32-bit backbuffers. LockRect stages GPU
pixels in guest memory in D3D byte order; writable UnlockRect uploads only the
locked rectangle in queue order after prior draws. Read-only locks preserve
GPU contents, and Reset rejects an outstanding backbuffer lock. The bridge
supports READONLY and NOSYSLOCK flags for these locks.

The renderer boundary is a CXX bridge. Rust owns typed device, resource and
storage state; C++ retains guest COM dispatch, memory validation and HRESULT
conversion. CXX-generated headers are build output and are not tracked.

The Metal headless probe passed clear/readback, transformed triangles and depth
occlusion on Apple M1 Max. Startup replay produced one 640x480 PNG, then failed
loudly at `IDirect3DDevice8::Reset`. This is bounded execution evidence, not game
playability or original-D3D8 equivalence. The current validation and exact kit
revision are recorded in the game repository's README.

## Layout

```text
src/d3d8/            reusable D3D8 layer (no windowing, no game types)
  enums.rs           D3DFORMAT / FVF / render-state / texture-stage / transform types
  math.rs            D3D row-major matrices and vectors
  format.rs          D3DFORMAT <-> wgpu::TextureFormat
  state.rs           render state, texture-stage state, transforms, viewport
  resource.rs        CPU storage, mip layouts, borrowed vertices, indexed preparation
  fixed_function.rs  FVF -> vertex layout and fixed-function WGSL
  device.rs          Clear / Draw* / Present / resource creation
src/backend/         wgpu instance / adapter / device / surface / readback
src/bridge.rs        CXX shared types, device/resource ownership and D3D7 state
src/probe/           shared probe harness and assertions
src/bin/             probe-headless (offscreen readback), probe-windowed (present)
```

## Probe milestones

1. Device/backend init on the real adapter; record backend, adapter, limits.
2. Exact full-target clear + readback.
3. Fixed-function FVF triangle + center-pixel color readback.
4. Present (windowed minimum; headless readback also supported).
5. Textures, then depth/stencil (`D3DFMT_D24S8` as a probe target).

Unsupported operations must fail with a named diagnostic, never silently succeed.

## Implementation handoff

- [Pinned D3D8 headers](../../third_party/wine-d3d8/README.md): unchanged Wine API references.

Shader-model 1.1 token translation and declarations are implemented alongside
the fixed-function stages. See [shader support and validation](SHADERS.md) for
the supported instruction slice and remaining fidelity gaps.

From the repository root:

```sh
python3 tools/test.py --d3d8-wgpu test
python3 tools/test.py --d3d8-wgpu probe
```

The probes exit 0 only when every requested check passed, 1 for a pixel/report
failure, or 2 for unavailable/unsupported operations. A missing requested check
or an unsupported result cannot count as a pass. Cargo outputs stay under the
ignored build tree.

Draw diagnostics: `RECOMP_D3D8_TRACE_DRAWS=<n>` prints up to `n` draws with
both stage bindings (texture id or unbound), `RECOMP_D3D8_TRACE_DRAWS_START=<n>`
skips that many draws first, and `RECOMP_D3D8_TRACE_STAGE1=1` restricts the trace
to draws with a stage-1 texture bound.

D3D9 logs shader-module and pipeline creation taking at least 50 ms, plus
shader/layout/pipeline cache clears, to stderr with the `[d3d9-compile]` prefix.
`RECOMP_D3D9_STATS=1` independently emits `[d3d9-stats]` counter deltas every
120 rendered frames per calling thread, plus a partial bucket at thread exit.
These cover draw-memo lookups/probes/evictions, index-range cache misses and
cleanup, texture-binding misses/invalidation, effect calls and CTAB parsing,
constant-register fills/temporary bytes, and buffer uploads that copy snapshots
or detach weak references. `frame` is the thread's cumulative rendered-frame
count; `frames` and `elapsed_ms` describe the bucket. Counts include attempted
operations; memo matches/rejects count candidates examined, not draws. Constant
fill counters exclude preshader writes; saved CTAB parses count actual cache fills.
Index cleanup counts FIFO entries visited, with dead removals and live evictions
reported separately. This mode adds no per-draw clock reads
or logs; use sampling separately for CPU costs and leave pipeline tracing off.
`RECOMP_D3D9_TRACE_PIPELINES=1` also logs every pipeline miss before and after
compilation, every module creation, and shader variants. Records include frame
numbers, source hashes and byte counts (vertex then pixel), pipeline recipe
hashes derived from complete CPU recipes, timings and cache sizes. Matching
recipe hashes expose recompilation across cache clears; missing source/layout
metadata is reported as `None`. Guest shader variant origins use content keys
(fingerprint plus full bytecode equality), so identical programs can share
variants across handles. Recipe identity now uses owned stage inputs/settings,
binding and vertex interfaces, entry points and raster/output descriptors, with
full equality available through its key. Recipe hashes from earlier revisions
differ even for unchanged rendering; generated WGSL source hashes remain
comparable. Source/layout handle lookups affect diagnostic metadata only,
not recipe identity.
The same flag records each drawn program's model,
numeric opcode histogram, encoded register indices, modifiers, declarations and
constant/texture metadata once per bytecode content, plus VS/PS content hashes
for every draw. These inventory records contain no bytecode or WGSL. Destination
indices describe decoded fields, including unused fields on control instructions;
they are not inferred register capacities. A schema/build identity includes a
fingerprint of renderer sources, manifest, lockfile and build script, target and
profile, plus enabled device features/limits. The normal adapter startup record
supplies backend and adapter identifiers. Draw records include execution mode,
interpreter eligibility and the prepared family; specialized-only mode leaves
eligibility unknown. The analyzer reports these alongside worker queue,
compilation, publication and saturation metrics, correlated by recipe identity.

`RECOMP_D3D9_SHADER_MODE=compare-family11` enables the M2 scoped comparison:
draws matching the supported SM2 program subset and baseline family-11 static
interface/raster state use the prepared interpreter; other draws keep
specialization. This is a scoped comparison, not whole-scene interpreter-only
coverage. A selected draw with missing preparation or unsupported resources fails
by name. `interpreter-only` rejects uncovered draws and never selects or schedules
specialization. Metal defaults to `hybrid`; other backends default to
`specialized`. Set `RECOMP_D3D9_SHADER_MODE=specialized` to roll back without
creating interpreter GPU resources. Startup logs the effective mode and backend.
In hybrid mode, covered misses render through the prepared interpreter
while one worker generates and compiles the complete specialization. Ready results
are adopted at draw boundaries. Ready specializations use native vertex fetch
where supported, otherwise the existing compact packer; interpreted draws use
canonical packed vertices. Uncovered draws retain synchronous specialization with
residual reasons. Other backends reject hybrid pending backend acceptance.
Unknown modes fail at
initialization. See [SHADERS.md](SHADERS.md) for exact coverage and bindings.

The worker admits 32 waiting recipes with an 8 MiB budget for estimated retained
recipe data and failure records. Successful completion releases that charge;
failed jobs keep their charge and a bounded diagnostic until reset, without retry.
Saturation keeps covered draws interpreted. OOM remains a renderer error and
physical device loss uses the existing loss path. Reset retires the worker's
private completion channel without joining compilation, so old results cannot
enter a new epoch. `[d3d9-async]` logs queue/compile/publication timings, fallback
residence, saturation, failures, epochs and residual counts.

Completed pipelines share a 512-entry, 32 MiB estimated recipe cache with
synchronous specialization. Shader variants and module caches each have the same
limits; worker binding layouts retain up to 128 entries. Recipe preparation and
interpreter program uploads each have a separate 512-entry, 8 MiB cache. These
caches evict the least recently accessed entry and refresh recency on hits.
Program upload charges include GPU buffer bytes and retained original bytecode.
Pending draws and draw memos keep their own resource references across eviction;
cache limits exclude those references, opaque driver allocations and temporary
compiler memory. These are retention limits, not performance acceptance budgets.
Routine preparation, compilation, upload and frame logs require
`RECOMP_D3D9_TRACE_PIPELINES=1`; startup mode selection and compilation failures
remain visible by default, as do evictions from caches that can require
recompilation.

Add `RECOMP_D3D9_UBER_TINT=1` to mix 15% red into interpreted fragment results
after raster output processing, preserving alpha. The tint is a diagnostic to
locate interpreted content; disable it for rendering comparisons. It is a
per-draw uniform flag and creates no additional pipeline variant. Startup records
include `tint=true` when enabled.
In hybrid mode the tint disappears when a ready specialization is selected.
For a visible handoff, `RECOMP_D3D9_ASYNC_DELAY_MS=5000` holds each successful
worker result for five seconds before publication. It defaults to zero, delays
only the compiler worker, and logs `debug-delay`; unset it for performance captures.

Interpreter modes prepare one module during device initialization, including
replacement devices: `compare-family11` creates one pipeline; hybrid and
interpreter-only create 43 captured pipeline states, retaining the UI/video and
additive families. The interpreter supports straight-line arithmetic, relative
vertex constants, seven packed inputs, two color/eight UV varyings, texture-free shaders
and five 2D textures plus two cube maps, projected 2D sampling, saturation,
partial-precision hints and deferred TEXKILL; see [SHADERS.md](SHADERS.md) for exact limits.
Program changes upload immutable data
and reuse these prepared pipelines.
`[d3d9-uber]` records include warmup time/manifest, program uploads and
resident buffer/retained-bytecode sizes, and per-frame selected/total draw counts.
With pipeline tracing, they also identify interpreted draws by program content,
prepared pipeline, target/subresource and texture; frame counters separate CPU
preparation and packing, packed bytes and logical constant/parameter bytes.
Logical uniform bytes precede batch deduplication; existing batch upload counters
report actual total uploads. CPU timings include diagnostic overhead and require
pipeline tracing; driver pipeline memory and GPU execution time are not measured.

`RECOMP_D3D9_UBER_CAPTURE=1` additionally logs original words and lowering once
per interpreted content or the known baseline candidate pair
(`dc32dcdca4f64c33` / `c49b28329a8ab277`), complete recipes/interfaces, and
frame/draw, target/subresource, viewport, primitive and texture identities. It
accepts `RECOMP_D3D9_UBER_CAPTURE_PAIR=VS_HEX:PS_HEX` to replace the baseline
candidate with another pair, or `all` to capture every encountered program once;
malformed selectors fail at initialization. It
performs no readback. Capture hashes select diagnostics only; interpreter admission
uses actual decoded programs and complete draw requirements. These records help
locate the content before a targeted visual/readback comparison.

`observed-family` records group the existing specialized pipeline descriptors
without shader contents or guest handles. They include resource layouts, input
linkage, stage variant state, vertex layouts and raster/output state. Family IDs
use content equality and remain valid across cache clears; hashes are diagnostic
fingerprints. `draw-family` records include memoized/cache-hit draws and correlate
families with source-based recipe hashes. These conservative observed families
are inventory, not interpreter eligibility or the eventual interpreter family
manifest. Current constant bank sizes and native vertex layouts can split them.

Summarize a timing/inventory log without launching the renderer:

```sh
 tools/.venv/bin/python recomp-kit/tools/analyze_d3d9_trace.py build/capture.log \
   --start 0 --count 600 > build/ubershader/report.json
```

Run from the game repository root and create the output directory first. Omit
`--count` to include all completed frames. The report ranks families by draw
count, lists their program pairs and attributed synchronous compile cost, and
reports p50/p95/p99/max frame counters. Compile costs are assigned to the draw
causing each miss, including shared modules; unattributed costs remain explicit.
Split captures at device replacement or process boundaries. The parser can
reassemble the observed audio-sink message interruption between `frame=` and its
numeric continuation, reporting the count explicitly; other malformed records
still fail. GPU timing remains
unavailable in this trace; rendering/performance acceptance requires separate
matched GPU measurements.

Per-frame records report successful nonempty draws, CPU time inside draw
preparation (including synchronous compilation, trace logging and any batch
flush), geometry preparation time and packed vertex bytes. Module and pipeline
CPU times are separate, overlapping components of draw CPU time. These records
are neither wall-clock frame durations nor GPU timings. Geometry time includes
input linkage/native-fetch selection as well as packing; packed bytes exclude
native streams, indices, constants and cached geometry uploads. Separate upload
byte counters measure cached geometry creation and staged draw batches (including
constants, streamed geometry, indices and alignment padding), attributed to the
frame of upload. Texture transfers are excluded. Timing traces retain observed
program bytecode and family metadata for content equality until device destruction; use bounded
capture sessions when measuring memory.

`RECOMP_D3D9_TRACE=1` logs draw state, shader bytecode/WGSL, constant values,
buffer hashes and decoded vertex samples. `RECOMP_D3D9_TRACE_FRAME_START=<n>`
selects a frame window; `RECOMP_D3D9_TRACE_FRAME_COUNT=<n>` defaults to 2 and
`RECOMP_D3D9_TRACE_DRAWS=<n>` limits draws per frame in that window (default 24).
Without a frame window, the limit applies to the first draws across all frames.
Tracing preserves native geometry submission; `RECOMP_D3D9_TRACE_READBACK=1`
additionally flushes selected draws and logs the target's nonblack pixel count.
Readback changes GPU synchronization; leave it off for the initial capture.

## Supported slice and remaining work

Full-target color/depth/stencil clears, material/light storage, render/texture-stage
state, world/view/projection transforms, viewport, scene boundaries, and unlit
triangle-list draws with FVF 0x42/0x142/0x242 are supported. `D3DPT_POINTLIST`
is also supported through wgpu's fixed one-pixel `PointList`, but only in D3D8's
default point state: a non-default `D3DRS_POINTSIZE`, `D3DRS_POINTSCALEENABLE`
or `D3DRS_POINTSPRITEENABLE` fails by name. FVFs 0x112
(XYZ/NORMAL/TEX1) and 0x152 (XYZ/NORMAL/DIFFUSE/TEX1) use GPU
diffuse/ambient/emissive vertex lighting (directional/point/spot), material
sources and inverse-transpose normals, with floating diffuse passed to the
texture/fog/alpha/depth raster path; 0x112 has no COLOR1, so the material
supplies the diffuse. Both indexed and nonindexed draws use this path,
including fixed-function vertices paired with pixel shaders. Immutable per-draw
uniforms retain light/material state and precompute light transforms and cone
cosines. `RECOMP_D3D8_CPU_LIGHTING=1` forces the CPU reference. Unsafe positional
attenuation, nonfinite state/positional inputs and `ProcessVertices` retain CPU
processing and its diagnostics. Specular draws remain unsupported.

`DIVERGENCE(original):` GPU rounding, normalization and `pow` can differ from
original D3D8 hardware and the CPU evaluator. Metal synthetic comparisons allow
at most two 8-bit channel levels per pixel; they cover both FVFs, both index
widths, sparse/repeated indices, nonzero offsets, directional/point/spot/mixed
lights, material sources, normal normalization, lighting-disabled output,
textures, pixel shaders, fog, alpha testing and queued snapshots. No original
D3D8 visual equivalence or gameplay performance gain is established.

D16/D24X8/D24S8/D32 map
onto wgpu depth formats; the backend-defined D24 precision remains a fidelity
caveat. Draws reject unsupported reached state by name.

CPU textures keep native texel layouts and stage locks through guest memory.
CreateImageSurface uses the same storage for standalone SYSTEMMEM surfaces,
including descriptors, locks, CopyRects and device retention. CopyRects accepts
the zero-count/null-array form for copying a whole surface, including compressed mips.
GPU sampling and indexed triangle lists use immutable draw snapshots, with
expansion available for other supported topologies and diagnostic comparisons.
Resource pool/usage,
LOD/priority, COM identity and binding bookkeeping still live in the bridge.
The optional no-Rust kit profile retains CPU storage in C++ and advertises no
renderer. Headless present completes the offscreen frame; the guest bridge
reads it back and presents through the host display seam.

Specular lighting, other FVFs, line and triangle-strip/fan topologies and resource lock
flags remain incomplete or unsupported. The light-index range is currently bounded to eight slots, an
implementation limit that must not be confused with D3D8's active-light capacity.
Guest COM execution, GPU probes and original-D3D8 comparisons are different
levels of evidence; the latter remain outstanding. Half-pixel raster coverage,
numerical precision and repeated-frame game behavior remain open.

### Render-target textures

Level-0 DEFAULT-pool A8R8G8B8/X8R8G8B8 color surfaces can be bound with the
implicit or standalone depth surfaces, or with depth detached. Setting a color surface resets
the viewport; depth-only changes keep it. A padded color attachment permits a
smaller color target to share the original larger depth surface; viewport-limited
clears/draws preserve depth outside that region. GPU copies publish the logical
color region after each write, including target switches within an open scene.
The implicit backbuffer remains the presentation/readback source.

Rendered levels are owned until their surface is destroyed, independently of
the CPU upload LRU. Sampling unchanged CPU generations reuses GPU-written pixels;
CPU generation changes upload new bytes. CPU staging reads synchronize from GPU
only when its generation is current. The bridge keys storage by level-surface
identity so releasing a parent texture cannot invalidate a still-bound surface.
Standalone depth storage uses the guest surface identity, dimensions and format;
it persists across bindings and is released with the surface. GPU allocation is
deferred until its first binding. Mip levels remain unsupported. A draw that
samples its own color target flushes the batch and samples a copy taken before
the draw (a divergence for overlapping primitives within that draw); sampling
the bound depth target fails by name. Stale bindings in stages the draw does not
sample are ignored.

The headless probe's `render texture/depth/restoration/readback` check covers
rendering, same-scene restoration, shared-depth preservation, sampling after
upload-cache eviction, CPU writes/readback generation ordering and depth detach.
This is GPU/API-model validation; original D3D8 differential equivalence remains
unmeasured.

Nonindexed triangle strips (D3DPT_TRIANGLESTRIP=5) and fans (=6), including
`DrawPrimitiveUP` from the guest bridge, expand into immutable triangle-list
uploads with preserved winding. The pre-transformed FVF 0x144 uses a 28-byte
XYZRHW/diffuse/UV layout and a vertex entry without a specular input.

Nonindexed line lists use wgpu LineList with triangle culling disabled.
Color-only XYZRHW/DIFFUSE (`0x44`) vertices use the existing screen-space
shader with a 20-byte layout and no texture/specular reads.
