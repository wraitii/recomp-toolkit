# Rustify plan

Build a modular Rust kit around the existing wgpu renderer: reusable D3D7/8/9
and D3DX behavior, a separate guest adapter, shared graphics infrastructure and
a Rust compositor/presenter. In parallel, consolidate the existing Rust
translator and move the p-code evaluator and execution core into Rust. Retain
SDL3, useful native libraries and generated C for translated game code.

The outcome is independently usable components with explicit ownership,
cross-platform support and localized upgrades. Moving code between languages
is useful when it advances those properties. The detailed source inventory is
in [RUSTIFY-preplan.md](RUSTIFY-preplan.md); this plan sets the delivery scope
and supersedes its tentative sequencing and choices.

## Scope and architecture

The principal release is the standalone graphics stack plus its kit integration
on macOS/Metal, Linux/Vulkan and Windows/DX12. Translator, interpreter and runtime
deliveries can progress independently; they do not gate the graphics release.
Preserve implemented behavior across retained APIs, with two explicit changes:
remove the renderer's process-wide single-device restriction and retire D3D11.
Remove D3D11-specific shims and host GPU work, preserving any shared services
still used elsewhere. Removed entry points fail by name if reached.

DirectDraw/GDI guest semantics and CPU surface operations may remain native.
Full DirectDraw migration, broader API/instruction coverage, Windows replacement
DLLs, multiple guest runtimes and a Rust code-generation backend are follow-on
projects. SDL remains the kit's window/event/input/audio integration; independent
graphics consumers choose their own host. Mobile and web are future delivery
targets whose constraints influence interfaces now, not promised release targets.

| Component | Owns | Boundary |
| --- | --- | --- |
| D3D library | Version-specific semantics, resources, state, shaders and reset | Ordinary Rust types and errors; no guest memory, COM identity, SDL or bridge dependency |
| Guest graphics adapter | Guest marshaling, HRESULT mapping, D3D staging and interface dispatch | Runtime memory/COM services and the D3D API |
| Graphics infrastructure | GPU context, device capabilities, submission/completion and image leases | Shared by D3D and compositor; no D3D semantics or kit policy |
| Compositor/presenter | Layer composition, layout snapshots, frame queue and presentation | Graphics infrastructure plus host surface/events; no guest memory access |
| Execution core | Guest memory, CPU/dispatch services, interpreter and scheduling | Generated C ABI, import/shim services and host capabilities |
| Translator | Image/lifting/analysis/emission pipeline | Reusable Rust entry points plus kit-specific CLI/configuration |

Keep D3D versions and shader implementation as modules in one library initially.
Use a small shared graphics crate for the context and lease contracts genuinely
needed by both renderer and compositor; avoid inventing another general GPU API
over wgpu. Keep rendering profiles and UI policy in the compositor/kit layer.
Windowing adapters, probes, D3DX helpers and native shader compilation are optional
features or companion packages. The kit enables the features required to preserve
its retained behavior.

Retain CXX where it is a useful private adapter. Use a small C ABI where native
consumers need an independently supported interface; replacing CXX is not a
milestone. Neither boundary leaks into the Rust library API. Version external
ABIs and persisted formats, while internal Rust interfaces evolve together.
Expose wgpu types only through an explicit embedding interface: those consumers
share its wgpu version, while guest/D3D semantics remain insulated from upgrades.

## Contracts established by the first working slices

These are short ownership agreements developed with implementation, not a
repository-wide design phase. Define enough for each slice, then extend it.
Exercise reusable library boundaries in the kit and an independent consumer;
exercise kit-specific services through their native and guest callers.

**Guest access and execution.** Use distinct checked 32-bit guest addresses,
with overflow, null, bounds and unaligned access behavior defined. Only the
scheduler baton holder mutates guest memory. No guest borrow survives a callback,
yield, arena content swap or asynchronous work. Separate graphics services
(memory/allocation, COM, display) from execution services (CPU state, dispatch,
imports, traps, checkpoints and yielding); share primitives without creating one
universal runtime interface. Both initially wrap the native runtime. Specify
reentrancy, allocation ownership and shutdown; no Rust panic or C++ exception
crosses an FFI boundary. OS services continue through `platform/os.h`.

**Objects and identity.** Rust devices own renderer resources and their reset
transactions. One shared registry owns guest COM identity and reference
transitions; initially it stays native. Preserve controlling `IUnknown`,
internal/external references, weak caches, recursive release and teardown after
guest heap reset. Renderer handles never double as guest COM IDs. Independent
renderer instances have independent mutable state and configuration, whether
they use separate GPU contexts or share one. Kit environment settings become
defaults at the adapter boundary, not process-wide library policy.

**GPU ownership and recovery.** Support owned contexts and caller-supplied
device/queue pairs. Validate capabilities, limits and device identity before
resource creation; define submission ordering and completion across renderer and
compositor. The context owner controls error handlers and physical device
replacement. On physical loss, stop affected submissions and report a recoverable
lost state. An owned context can stage a replacement; a supplied context requires
the owner to provide one or reconstruct its clients. Coordinate shared-context
recovery across all clients and invalidate resources by context generation.
D3D reset stages changes and commits atomically to logical state; failure does
not partially install resources, and does not imply a lost GPU becomes usable.

**Images, composition and presentation.** A rendered image carries dimensions,
format/color/alpha interpretation, readiness and a retained resource lease.
A composition submission separately carries world/UI/overlay layers, rendering
profile and layout inputs. The compositor publishes an immutable layout snapshot
for input mapping; a flat image must not replace that information. The presenter
owns acquire/present, resize and pacing; the host owns the window and its lifetime.
Specify release for completion, dropped frames, cancellation and shutdown.
Use shared-device textures in the kit, CPU readback as the portable fallback,
and optional native texture sharing for integrations that need it. Asynchronous
work retains resources and owned bytes, never guest pointers.

**DirectDraw surface coherence.** Keep native DirectDraw storage behind an
interface describing formats, pitch, palettes, attachments, authoritative bytes
and synchronization at lock, draw, blit, flip, GDI access and writeback. Revisions
track observed changes; they cannot detect arbitrary writes through retained
guest pointers. For exposed writable storage, conservatively snapshot or compare
bytes at consuming boundaries. Resolve CPU/GPU authority before overwriting
either copy and state the ordering when writes overlap. Provide library-owned
surfaces for standalone D3D7. Record current behavior first, then fix the known
mixed 2D/3D coherence gap as a separate compatibility change with original
evidence. The new contract must support that fix; it must not encode the gap as
the intended behavior.

## Parallel delivery

Milestones are integration points, not a serial queue. Packaging and platform
qualification run throughout. Teams can work independently after agreeing on
the small interfaces they share; integrate each complete operation family before
expanding it.

| Track | Starts with | Depends on | Delivery |
| --- | --- | --- | --- |
| Build and portability | Existing renderer and dependency audit | No migration prerequisite | Target builds, feature isolation and continuous backend evidence |
| G1: D3D9 slice | CPU resource lifecycle, then draw/output/reset | Initial resource/context contracts | Same Rust API used by kit and independent consumer |
| G2: API and guest migration | Each completed G1 operation family | That API plus native runtime facade | Retained D3D7/8/9/D3DX behavior behind Rust guest adapters |
| S: D3D7 surfaces | Existing DirectDraw/D3D7 flow | Surface/COM contracts | Guest-backed and standalone surface integrations |
| P: Presentation | Image leases and current host frame flow | Shared graphics contracts, not all of G2 | Rust/wgpu compositor and presenter |
| T: Translator reuse | Existing Rust workspace | No graphics dependency | Library entry points and deterministic CLI output |
| E: Interpreter | Current evaluator/lifter and native runtime | Execution contract, not graphics facade | Rust fallback integrated with translated execution |
| R: Runtime ownership | Lifecycle mapping and native service facades | Contracts for each subsystem | Memory/loader, dispatch, COM and scheduler deliveries |

### G1 — Prove the graphics boundary end to end

Move semantic types and ownership out of `bridge.rs` for one useful D3D9 slice:
create a device and resource, lock/update, bind/draw, obtain an image, reset and
release. Include failure and teardown paths. Adapt current native callers to
the same API, without waiting for their guest marshaling to move to Rust.

Build a small independent Rust consumer immediately. Exercise offscreen output
and both owned and supplied context construction; verify two independent
renderer instances. Start desktop target builds and backend runs with this
slice. Keep all unmigrated operation families working through the existing path.

**Done:** a complete useful path works through ordinary Rust types in both
consumers, including reset/release. It has no guest or CXX dependency in its
public model. This proves the design; full API coverage belongs to G2.

### G2 and S — Complete graphics semantics and guest integration

Expand by coherent families: resources/views, locks, bindings/state, shaders,
draw submission, device lifecycle and implemented D3DX. Move each family's
marshaling, HRESULT mapping, staging and dispatch into the guest adapter as its
Rust API settles. Generate interface definitions from pinned inputs, preserving
slot order, signatures and named unsupported diagnostics. Keep native compiler
dependencies behind a standalone compiler interface.

Run D3D7 surface work alongside D3D8/9 migration; establish its authority and
lifetime contract before finalizing D3D7 resource APIs. Keep shared COM native
until the independent registry migration is ready. Retire each replaced path
after switching and checking its callers, rather than maintaining two semantic
implementations indefinitely.

**Done:** all retained implemented D3D7/8/9/D3DX operations use the shared Rust
library, and the kit reaches it through the Rust guest adapter. Native DirectDraw
and remaining COM consumers work through explicit services. Baseline behavior
and separately reviewed compatibility fixes are distinguishable.

### P — Consolidate graphics execution and presentation

Deliver three increments: first image leases/offscreen output and an adapter
to the current presenter; then Rust composition and legacy host GPU command
execution; then surface presentation through the retained SDL host and an
independent host. Support caller-supplied contexts from G1 rather than redesigning
resource ownership here. External consumers can bypass the kit compositor.

Preserve layered rendering profiles, overlays, coordinate mapping, resize,
pacing, GDI readback and sealed-frame lifetime. Retire D3D11 and then duplicate
host GPU implementations once all retained consumers have replacements. Preserve
CPU-only/null-host operation and useful fake-backend checks; backend consolidation
must not make every resource or host check require a GPU.

**Done:** retained host graphics run through Rust/wgpu; SDL supplies native host
services. Both direct image embedding and layered kit presentation work, with
completion, dropped frames and shutdown accounted for.

### Graphics release — Join the completed tracks

Finish G2/S and P, package the same library used by the kit, and qualify the
retained operation inventory on macOS/Metal, Linux/Vulkan and Windows/DX12.
Track build, resource/shader behavior, offscreen rendering, presentation and
guest integration separately. Missing platform runs remain incomplete entries;
they do not reduce the release scope. Existing unsupported methods remain named
failures, and D3D11 retirement is recorded as an intentional removal.

Start build work with G1: isolate bridge/windowing/compiler dependencies, preserve
shader generation and fingerprints, centralize compatible wgpu/naga pins, and
support explicit Cargo targets and staged dependencies through `tools/build.py`
and native CMake orchestration. Complete metadata and license obligations.
Document the external package's consumption path and an upgrade procedure that
exercises both the kit and independent consumer.

Mobile/web constraints are reviewed early: window/surface ownership, blocking
operations, worker availability and native compiler/lifter dependencies stay
behind explicit capabilities. Desktop qualification does not imply those targets
work. Their implementation and qualification remain separate deliveries.

### T and E — Reuse translation and move fallback execution to Rust

**T:** expose image, lifting and translation entry points from the existing Rust
workspace. Keep kit configuration, native-header conventions and output layout
at the edges. Preserve useful crate boundaries, versioned inputs and deterministic
generated C. Done when the CLI and an independent library caller use the same
pipeline without duplicating orchestration.

**E:** port `runtime/pcode.cpp`'s evaluator beside the Rust SLEIGH lifter, then
move `pcode_x86.cpp`'s CPU/dispatch adaptation behind execution services. Share
the lifted operation format and decoding infrastructure, without linking offline
analysis or emission into the running game. Preserve byte-validated caches,
nested execution, checkpoints, import/translated dispatch and named unsupported
semantics. Retain the CPU/register, flags, x87, stack, hooks and trap contracts
shared with `runtime/x86.h`; this does not expand interpreter instruction coverage.

Done when Rust fallback execution preserves current behavior against the native
runtime and integrates with generated C. Qualify native lifter packaging on the
desktop targets, including Windows. Choose static or dynamic linkage according
to distribution needs; neither is a success criterion. T and E can finish
separately and do not wait for graphics migration or a Rust scheduler.

### R — Move runtime ownership in separate deliveries

Establish initialization, reset, callback and teardown ownership for each service
before switching its implementation. Deliver independently where dependencies
allow:

1. **Memory and loading:** Rust owns guest arena/heap and PE loading, preserving
   the arena base, content-swap behavior and generated memory access contract.
2. **Dispatch and imports:** Rust owns routing and import plumbing behind the
   existing execution ABI, preserving hooks, diagnostics and native shims.
3. **Shared COM:** Rust owns the single identity registry, with adapters for all
   remaining graphics/audio/native consumers. Its prerequisite is their lifecycle
   contract, not completion of every memory or interpreter port.
4. **Scheduling:** migrate the baton, thread/wait services, blocking imports,
   reentrant callbacks and shutdown together as one coherent delivery.

Use explicit contexts as the ownership target; transitional globals adapt
generated accessors without claiming multiple guest runtimes. Each delivery is
done when its native callers and Rust consumers use the same service and the
old owner is removed. Together these complete the required execution core.

Controller/settings models, audio mixing and further Win32 services are optional
extensions selected by reuse. Preserve existing C/Lua mod contracts, callback
phases and unload behavior throughout. SDL can remain the device sink, and the
launcher and remaining native shims need not be rewritten.

## Evidence and completion

At the start of each slice, record its operation/platform inventory, current
behavior and representative workload. Map each new contract to evidence from
existing checks, the independent consumer or app review: creation/failure,
reset/loss, release order, shared devices, frame drops, callbacks and shutdown.
An uncovered contract remains an explicit qualification task; compilation alone
does not establish it. Use original behavior to assess compatibility fixes.

Record comparable frame time, CPU cost, copies/readbacks, memory and build/shader
compilation cost before and after affected changes. No unexplained correctness
or material performance regression closes a milestone. Set workload-specific
budgets from the baseline before switching paths; document accepted tradeoffs.

Use existing checks first; do not write tests unless asked, per
[AGENTS.md](AGENTS.md). Identify missing evidence and the smallest additional
validation needed rather than assuming existing suites cover new contracts.
Build only through `tools/build.py`; rebuild the windowed host for app review.
Arrange approval for app/GPU-probe execution unless already authorized. Translator
changes run translator cases first, then regenerate and use the bounded paired
diff workflow in AGENTS.md, checking at most 200–300 functions.

Keep game-specific addresses/policy in configuration and generated hooks. Never
edit generated code to implement a migration, and never commit game files,
generated output, binaries or logs. Keep each slice reviewable, integrate it,
remove its replaced implementation, and update the coverage record.

## G1 implementation and evidence

The ordinary Rust D3D9 slice now supports owned and supplied contexts, buffer
creation/lock/update, binding, triangle drawing, CPU image output, reset and
release. Native callers share resource, construction, binding, draw, clear and
readback implementations. Reset uses the existing staged transaction internally.
The process-wide device restriction is removed. Shared clients use one context
submission coordinator and physical-loss token, with separate logical reset and
shader configuration. Kit environment settings enter at the native adapter.

The independent CPU consumer passes buffer lifecycle and invalid-input checks.
The Metal GPU consumer passes owned and supplied construction, two clients on
one supplied context, colored triangles and background readback, invalid and
stale handles, default-resource reset failure/retry, reset isolation, client
release/recreation, simulated shared loss, owner-supplied replacement and redraw.
Retained images own their bytes across reset and teardown. Run these through
`tools/build.py --renderer-consumer run` and `--renderer-consumer gpu-run`;
`build` and `gpu-build` compile without executing.

The GPU consumer exposed a readback ordering defect: queued draw/copy work had
not reached the GPU queue before direct CPU readback. Synchronizing the shared
submission coordinator before readback corrected the black output. Reset now
has an infallible logical commit after readiness; loss during commit remains
visible to subsequent operations instead of returning failure after installation.
The exact loss-between-ready-and-finish interleaving is reviewed, not injected.

Windowing and probe dependencies are optional; independent consumers build with
neither. CXX remains a private build dependency. Shader generation is retained,
feature selections enter fingerprints, and compatible wgpu/naga pins are shared.
Explicit native Cargo targets work; mismatched targets are rejected. The earlier
macOS ARM64 stub app build passed. The RT3 D3D8 app regenerated, compiled, linked
and bundled successfully at `build/railroad-tycoon-3/RailroadTycoon3Recomp.app`.
The user subsequently ran RT3 and reported that it looked fine. This adds a
user-observed D3D8 runtime checkpoint; it does not qualify the D3D9 guest path
or the other desktop backends.

A warm-cache Metal run measured 115.613 ms to create three clients, 17.524 ms for
two draw/readbacks and 2.893 ms for two resets, with 16,384 readback bytes per
64x64 frame. These single-run observations establish a workload, not a comparable
before/after performance result or an accepted regression budget.

G1 qualification remains open: Linux/Vulkan and Windows/DX12 runs, comparable
performance baselines and guest D3D9 workload evidence are missing. Raw supplied
adapter/device/queue provenance is a caller guarantee; capability checks do not
prove pairing. Owners must import once and serialize external queue access with
renderer work. Standalone nested buffer locks fail by name; the native nested
lock implementation is retained. CPU image output is verified; GPU leases have
the evidence below. The native presenter adapter and cancellation on physical loss remain presentation work.
Probe-only compilation now passes through `--renderer-consumer probe-build`;
staged-dependency targets and full bridge isolation remain packaging tasks.

## P offscreen lease implementation and evidence

`Device::present_gpu_image` returns a retained texture snapshot with dimensions,
format/color/alpha metadata and its originating context. It performs one GPU
copy and drains the shared submission worker before return; reads submitted to
that context's queue are ordered, but the GPU need not have completed. The lease
retains its texture and context across later presents, logical reset and renderer
teardown. Cross-context access and physical loss return named errors. Embedders
must not mutate or destroy the exported texture, must retain it through read
submission and must serialize external queue access with renderer work.

The Metal consumer verifies full-image equality with CPU output, retention after
two subsequent frames, reset and renderer teardown, foreign-context rejection,
loss and shared-context replacement. CPU lifecycle checks and the headless probe
build with windowing disabled also pass. RT3 was rebuilt and bundled after these
changes. The later G2 build received user review, recorded below. A single
warm-cache observation measured
611.417 microseconds for CPU present and 245.792 microseconds for lease creation
on the same 64x64 scene. CPU output reads back 16,384 bytes; lease creation uses
one GPU copy without readback. Consumer verification separately reads the lease
back. These timings do not establish a performance budget or regression result.

Validation exposed colliding wgpu device IDs across independent instances. The
native presentation ring and resource ownership checks now use retained context
and logical-device tokens instead of raw device equality. Raw supplied-handle
provenance remains the caller's guarantee. The native ring recovery correction
is reviewed and compiled; the offscreen consumer does not exercise that ring.

This delivers the offscreen lease portion of P. Adapting the native presenter,
extracting shared graphics infrastructure, composition and surface presentation
remain open, including native frame cancellation during physical loss. Worker
submission failures retain the existing behavior; record-time error scopes do
not qualify every asynchronous failure path. The following G2 slice extends the
ordinary API while preserving native COM identity.

## G2 2D texture slice

The current operation inventory is 2D texture creation and mip descriptors,
rectangle lock/staging/unlock/upload, texture/sampler/stage binding, release and
reset participation. Existing `resource.rs` semantics remain authoritative for
this migration: full-level staging retains pitch and rectangle offsets, writes
outside the locked rectangle are ignored, and unlock closure is distinct from
upload success. Native COM identity and guest staging remain native; cube/volume
views and D3D7 surface authority are outside this slice. The representative
workload is a textured triangle plus partial and mip-level updates in the
independent Metal consumer, alongside existing buffer/lease/recovery checks.
The ordinary API now exposes this family. Native and Rust callers share create,
lock preparation, unlock/upload/autogen and binding semantics. Unlock returns
both closure and status, so upload failure cannot hide that a lock closed.
Standalone texture mutation flushes queued draws. Released handles become stale,
while bound resources remain registered until unbound; this also applies to
standalone buffers. Settlement precedes release mutations, and cleanup after a
successful reset cannot turn the committed reset into failure.

The Metal consumer passes mip descriptors, full and partial locks, unchanged
bytes outside the rectangle and in another mip, read-only locks, invalid and
nested locks, queued draw-before-update ordering, texture sampling, stale handle
rejection and release while bound. DEFAULT textures block reset until released
and unbound. Managed textures retain CPU bytes through logical reset and physical
context replacement; sampling after replacement also passes, verifying GPU
reupload and binding. Autogen availability reporting shares the native predicate.
The existing CPU consumer passes; it remains buffer-focused. RT3 rebuilt and
bundled successfully with the shared native helpers. The user ran this final
D3D8 build and reported that it looked fine. This is user-observed integration
evidence, not automated or guest D3D9 texture qualification.

One warm-cache 4x4 A8R8G8B8/three-mip lifecycle with two textured draws/readbacks
measured 3.727 ms. An earlier 166.64 ms run included 163.52 ms of first pipeline
compilation. These are workload observations, not a performance comparison.
A CPU-only texture constructor has not been implemented; the new texture API
requires an initialized device. SYSTEMMEM/SCRATCH binding, NO_DIRTY_UPDATE,
autogen and injected upload failure remain unqualified, as do guest D3D9 texture
workloads and the other desktop backends. Native guest marshaling/HRESULT/staging
has not moved to Rust, so this is a G2 API family, not completion of G2.


## P native frame ownership slice

D3D7/8/9 native frames now transfer one shared opaque owned token to the host.
It retains the ring slot, texture, readiness event and GPU device/queue until
rejection or host copy completion; D3D9 additionally retains its context and
submission worker. The null host also consumes rejected tokens. Completion
releases the imported texture and token before retiring the presenter's tracked
command. Ring replacement and teardown drop their own references without waiting
for, or leaking, borrowed busy pointers. The old borrowed-pointer host seam and
D3D7/8 leaked busy allocations are removed. D3D7 stages without sealing, keeping
its layered DirectDraw flow; D3D8/9 use the owned present endpoint. D3D7's RGB565
conversion, CPU fallback and GDI behavior remain on their existing paths.

Normal pacing remains asynchronous: three reusable producer slots, one producer
GPU copy and one host GPU copy, with the existing native readiness event. The
new path adds a token allocation per frame, not a texture allocation. Exceptional
host staging/registration failure drains encoded work before releasing resources.
No comparable native latency baseline was captured. Loss before readiness can
still leave an unsignaled wait and stall the host queue or shutdown, including
when staging declines after encoding a wait. This is ownership progress, not
completion of P or its cancellation contract.

The RT3 app rebuilt without being launched. Existing fake presenter checks
(311), offscreen Metal presenter checks (134), and CPU/GPU consumers pass.
These do not directly exercise the owned native seam or its rejection,
resize and shutdown paths. The user subsequently reported that RT3 still works
and supplied a symbolicated Samply capture (2026-10-29 13:38), whose stacks reach
the owned D3D8 handoff. In the stable 21.5–23.7 s 3D interval, `Dev_Present`
accounts for approximately 14.6 ms of 1,906.9 ms sampled main-thread CPU (0.8%),
including 7.4 ms in the Rust handoff, with no sampled ring-slot wait. This is
normal-path evidence, not loss/shutdown qualification or a before/after latency
comparison. Frame markers measure guest seal cadence, not display completion.
Test-binary compilation passed with
p-code disabled; the p-code-enabled configuration encountered unresolved symbols
in `interp_tests`. The final RT3 app build retained p-code. No tests were added
and no translator code changed.

## Next tranche handoff

The current checkpoint includes G1's useful API path, P's offscreen lease and
native frame ownership slice, and G2's 2D texture API family. Metal consumers pass;
the user reviewed both the texture and owned-handoff tranches in RT3, with a
sampled normal-path profile for the latter. Full G1 qualification, G2 guest migration and
P presentation remain open. Build artifacts and game installations remain ignored.

Next, qualify the owned native handoff through the host. Resolve loss before
readiness without hanging a GPU wait or exposing
an invalid frame. Preserve layered sealed frames, GDI readback and CPU fallback.
Capture the existing present workload and latency/copy baseline before switching
paths; a blocking completion wait requires an explicit performance tradeoff.
Acceptance needs normal presentation plus resize, retained frames, rejection,
loss and shutdown evidence through the host, not only the offscreen consumer.

The readiness audit found that `pending_prefixes` gates retirement, not
composition: the worker currently relies on GPU queue order. The next change
needs a per-frame readiness gate before composition, with immutable destination,
source lease and generation captured while holding the guest baton. Submit host
copies only after successful producer completion, in guest staging order even
when callbacks arrive out of order. Failure drops the frame without inventing
readiness; frame drop and shutdown cancel unsubmitted sources, while submitted
copies retain their leases through completion. Wake the presenter on each
transition. Keep GDI's synchronized fetch independent and do not access guest
memory from readiness callbacks. Replacing the native event wait alone is
insufficient.

In parallel, implement G2's first guest adapter operation family against explicit
checked memory/allocation and native COM services. Start with buffer lifecycle
and staging, then reuse that facade for textures; keep shared COM identity native
and retire each replaced marshaling path after its callers switch. Do not expand
cube/volume APIs or D3D7 surface migration before their lifetime contracts are
established. The buffer inventory includes full-buffer aligned guest staging,
nested writable locks, draw-time synchronization, retained staging budgets,
allocation rollback and heap-reset teardown. Preserve these through the checked
facade while native COM owns identity and recursive release. Carry the
qualification gaps above into each slice rather than
marking the graphics release complete from Metal or RT3 alone.

Resume with `tools/build.py --renderer-consumer run`, `gpu-run` for the GPU
consumer, and `probe-build` for the no-windowing probe build when relevant changes
justify rerunning them. Build the integration app with
`tools/build.py --game railroad-tycoon-3 --target app --jobs 8`. Translator code
was unchanged in this tranche; no translator regeneration or differential run is
needed for these presentation changes.
