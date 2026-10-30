# Rustify plan

Finish the Rust presenter and graphics consolidation, preserve the kit's existing
behavior, remove replaced ownership paths, and prepare recomp-kit for upstream.
The kit is the primary product. Modularity should make its ownership and
maintenance simpler; independent library use is not an upstream requirement.

This plan replaces the earlier G1/G2/S/P merge sequencing. The source inventory
in [RUSTIFY-preplan.md](RUSTIFY-preplan.md) is historical context, not a checklist.

## Current state

The kit renders and composes through one shared wgpu context. Rust owns GPU
execution, composition planning, surface presentation and migrated D3D resource
operations. Replaced native Metal/Vulkan/WebGPU backends are removed. D3D11 is
intentionally retired; its entry points fail by name.

Rust now owns the sealed-frame mailbox, submission/retirement queues, completion
and prefix state, counters, retained repeat snapshot, worker, refresh/offscreen
scheduling, surface lifecycle and capture delivery. The Rust turn driver owns
admission, pacing permits, fallback checks, drawable retries, composition reuse,
encoding order and present/commit ordering. Native adapters retain sealed payloads,
translate individual GPU operations and publish runtime results.
Native guest COM identity, DirectDraw CPU operations, frame sealing and runtime
integration remain intentional boundaries. Full D3D guest migration is separate work. The C++ boundary is not yet thin:
it retains COM identity/refcounts/destruction, binding-to-object reconciliation,
guest marshaling and unmigrated API behavior. Direct COM-call acceleration from
origin changes dispatch, not ownership; see `dx/README.md` for the boundary.

The app builds and signs. Existing Metal graphics suites and independent
consumers passed at the latest checkpoint; a short scripted app run exercised
settings, frame completion and clean exit. The user reports RT3 and Civ4/D3D9
work correctly after origin reconciliation; workloads and durations were not
recorded. Physical-loss recovery and comparable performance remain unverified.
The FPS overlay uses
GPU completion rates because wgpu supplies no drawable display timestamps.

Reconciliation checkpoint (2026-10-30, origin `f16883e9`): the combined tree
builds the regenerated RT3 app and passes 11 selected native/graphics suites plus
29 translator cases at 200 inputs each. The paired RT3 diff sample of 200 functions
has 110 passing, 81 unreached and nine oracle-divergent functions, with no pair
discrepancies. Report: `build/railroad-tycoon-3/recomp/diff/paired-cvx2320d/report.json`.
Rebuilding the harness from origin `f16883e9` reproduces all 200 results exactly
apart from timing, reusing the generated archive: translator, game inputs and
harness source dependencies match origin. This isolates the failures from the
graphics changes, but is not an independently regenerated origin build or a clean
diff qualification; the nine discrepancies still need translator/harness triage. Both regenerated game apps build, and the user reports successful
RT3 and Civ4 gameplay after reconciliation. Portable packaging remains open.

## Upstream scope

Retain rendering profiles, world/UI/overlay composition, input coordinate mapping,
settings, capture/readback, frame pacing and existing graphics API behavior.
Preserve CPU-only/null-host operation and useful fake-backend checks. D3D11
retirement includes DXGI factories and the narrow D3DCompile tagged-source path;
that path recognized three exact shaders rather than compiling general HLSL.
D3D9 D3DXCompileShader through vkd3d-shader remains supported within its existing
limits. Unsupported entry points fail by name.

Full Rust presenter ownership means Rust owns the frame queue and worker,
selection, repeats/drops, pacing, submission/retirement, cancellation and surface
lifecycle. Native code seals guest frames, releases guest resources, supplies SDL
window/events and concrete platform timing, and delivers runtime callbacks. The
presenter consumes owned frame data and retained leases, never guest pointers.

Keep one shared device/queue for renderer and presenter. Make context, callback
and shutdown ownership explicit. Organize generic GPU execution separately from
D3D semantics, creating another crate only if it simplifies the kit. Standalone
SDKs, new graphics features, broader API coverage and automatic recovery of a
running guest after physical device loss are outside this checkpoint. Preserve
existing reset/recovery behavior and fail clearly where recovery is unsupported.

## Upstream milestones

Complete the implementation milestones, then hand the final tree to the user
for verification. Fix failures found there before declaring it ready to push.

### 1. Finish Rust presenter ownership — implementation complete

The Rust `Presenter` owns mailbox/timeline policy; its turn driver sequences the
native resource and runtime adapters. Replaced native policy and standalone
mailbox/timeline ownership APIs are removed. Existing compositor, host and
headless checks pass. Final runtime qualification remains in milestone 4.

- Move queue/worker ownership and the complete presenter state machine into Rust:
  selection, repeats/drops, pacing, submission, completion, retirement and
  cancellation. Rust also owns acquire/present, deferred resize and surface teardown.
- Replace native presenter policy with narrow frame/event and callback adapters.
  Keep guest sealing, guest-resource release and runtime/SDL integration native;
  preserve scheduler baton, immutable layout and sealed-frame contracts.
- Define transfer and release of frame/image leases for queued, submitted,
  repeated, dropped and cancelled work. Serialize state transitions and callbacks
  so readback, reentrancy, loss and shutdown have explicit lifetime rules.
- Carry timing capabilities into pacing and metrics. Distinguish successful GPU
  completions from actual display timestamps; preserve new/repeated frame counts.
  Add native timing glue only where needed to preserve acceptable kit behavior.
- Preserve the retained composition, input mapping, overlays, capture and headless
  paths. Keep the useful fake integration for deterministic lifecycle checks.
- Remove the replaced native queue/state machine and redundant Rust helper APIs
  once their callers use the Rust presenter.

Done when the kit's presenter has one Rust owner for policy, state and frame
lifetimes, and native code only performs the integration responsibilities above.
An independently reusable presenter API is not required.

### 2. Close graphics migration seams and remove leftovers — implementation complete

Kit context creation, ordered host submissions and native texture-lease imports
now have an explicit adapter. Generic execution and polling retain the shared
physical context; the queue owner preserves renderer lifetime and flushes existing
D3D9 work without starting an unused submit worker. Migrated D3DX matrix operations
have one Rust implementation across DLL versions. Native surface ownership, stale
backend hooks and redundant bridge fallback arguments are cleaned up; the retained
D3D ownership map is in `dx/README.md`. Final runtime qualification remains in milestone 4.

- Settle context creation/sharing, submission ordering, callback ownership and
  shutdown across renderer, presenter and host adapters. Remove hidden renderer
  dependencies where they obscure generic GPU lifetimes.
- Record a concise ownership map for retained D3D operations. Each migrated family
  has one semantic implementation; guest marshaling and resource ownership agree
  with its native callers. Finish handoffs needed by current kit behavior.
- Leave coherent unmigrated D3D families native. Preserve DirectDraw/GDI CPU
  behavior and shared COM services; full D3D7/8/9/D3DX migration is not a gate.
- Remove replaced implementations, unused dependencies, stale switches and obsolete
  shader/build paths. Keep named diagnostics for unsupported and retired APIs.
  Review compatibility changes separately from ownership cleanup.

Done when retained graphics paths have intentional owners, the shared-context
integration is explicit, and no abandoned migration scaffolding or competing
implementation remains.

### 3. Align builds, packaging and documentation — implementation complete

The graphics crates share one workspace and lockfile. Cargo diagnostics/link
metadata and native target/deployment settings have shared build helpers; separate
archives retain CPU FFI versus GPU feature boundaries. The packaged kit stages
vkd3d-shader with its headers, licenses and source, avoiding a Flex/Bison dependency.
Packaging includes new
source, omits deleted/ignored artifacts, and records source and graphics hashes.
Terra's readability review led to named bridge flags, one native-pending query,
corrected ownership/build docs and an explicit kit-only graphics build boundary.
Per-game renderer overrides are rejected because they could mix incompatible
native headers and Rust libraries. Full portable-package and runtime verification
remain in milestone 4; commands and comparison inputs are in `docs/testing.md`.

- Align Cargo dependencies/version pins, CMake targets and staged dependencies
  with the settled graphics boundaries. Consolidate workspaces where it removes
  duplicated orchestration; avoid restructuring unrelated Rust projects.
- Keep bridge details private to kit integration. Document ownership and failure
  behavior at interfaces without introducing a standalone SDK or unnecessary
  stable external ABI commitments.
- Ensure kit packaging carries the libraries, headers, licenses and metadata it
  actually needs. Use the configured Python environment and pinned requirements.
- Update architecture and build instructions to describe the resulting kit.
  Replace historical progress narratives with current behavior and explicit gaps.
- Prepare an upstream review summary covering ownership changes, removed paths,
  intentional behavior changes and remaining limitations. Include new source in
  the review; exclude game files, profiles, logs and generated/build artifacts.

Done when source builds, distributed kit contents and documentation describe the
same implementation, and the change can be reviewed without this conversation.

### 4. User verification and upstream decision

Reconcile current origin/main before final verification, preserving its D3D9
binding snapshots, effect/draw improvements and runtime/translator changes alongside
the Rust guest staging and presenter ownership. Run existing automated checks on
the combined tree; the user performs gameplay verification after it is settled:

- Existing native/graphics suites and the relevant packaged-kit build path.
- Representative gameplay, rendering profiles, settings, overlays and F10/F11 counters.
- Input coordinate mapping, capture/readback and CPU-only/headless behavior;
  capture callbacks that query the presenter or request shutdown.
- Resize, minimize/restore, dropped/repeated frames, cancellation and shutdown
  with work in flight.
- Reset/loss behavior: qualify recovery that the kit supports, and ensure unsupported
  recovery fails clearly without hanging or retaining invalid resources.
- Comparable CPU/frame cost, pacing, copies/readbacks and memory for a fixed workload.
  Completion timing must not be presented as measured display latency.

Supply exact commands, workload/baseline details and expected outcomes at handoff.
Keep unexercised cases explicit. No unexplained correctness or material performance
regression closes this milestone. macOS/Metal is the initial qualification target;
Native Linux/Vulkan and Windows/DX12 remain unqualified until exercised.
Cross-compilation (including mobile/web and multi-architecture builds) is currently
rejected by the Rust graphics integration; restoring those build paths is separate
work, not merely a testing task.

## Larger roadmap after this checkpoint

Recovered from `89437893:RUSTIFY-plan.md`. These are separate future milestones,
not requirements to extend the current graphics consolidation before merging.
Their order expresses dependencies; implementation has not been completed here.

1. **Finish retained guest graphics (G2/S).** Move remaining D3D7/8/9/D3DX
   semantics behind narrow guest adapters and settle DirectDraw CPU/GPU surface
   coherence. Done when each migrated family has one owner and existing games,
   locks, aliases and reset behavior remain correct. Preserve conservative handling
   of guest writes through retained pointers.
2. **Qualify the graphics boundary (P).** Establish desktop Metal/Vulkan/DX12
   behavior and lifecycle/performance evidence, and restore cross-target builds
   separately. The earlier independently reusable renderer/presenter objective
   remains a future option; the current checkpoint promises kit integration,
   not a standalone SDK. Automatic physical-device recovery is separate work.
3. **Reusable translator (T).** Expose image analysis and translation through
   coherent library interfaces while preserving generated C and reproducibility.
   Done when a non-CLI caller can use the pipeline and translator cases plus the
   bounded diff harness pass.
4. **Rust fallback execution (E).** Move p-code fallback execution into Rust while
   preserving CPU state, guest memory, dispatch, callbacks and checkpoint behavior.
   Done when translated and fallback execution retain their shared contracts.
5. **Runtime ownership (R).** Deliver memory/PE loading, dispatch/imports, shared
   COM identity and scheduler/thread ownership in separate steps. Each step must
   preserve 32-bit guest addresses, scheduler baton rules and explicit teardown.
   Multiple independent runtimes are not implied by this migration.

Keep each delivery bounded by a useful kit outcome. Retain generated C, SDL host
services and useful native dependencies. Follow [AGENTS.md](AGENTS.md): no new tests
unless requested, builds through `tools/build.py`, named unsupported diagnostics,
and no game policy in generic runtime/graphics code. Implementation checks may
support development; they do not replace the user's final verification.

## Upstream review scope

- Rust owns physical GPU context/loss tracking, generic execution, composition and
  presenter policy. Native code retains guest COM/memory integration, sealed
  payload adapters, SDL/events and kit overlay drawing.
- Removed the replaced native Metal/Vulkan/WebGPU implementations, obsolete shader
  build paths and native presenter policy. Retained DirectDraw/GDI CPU work and
  coherent unmigrated D3D/D3DX operations; migrated matrix math has one Rust owner.
- Compatibility changes: D3D11/DXGI factory/D3DCompile imports fail by name;
  per-game renderer-directory overrides fail at configure time. The FPS overlay
  labels successful GPU completion rate CMP because wgpu supplies no display timestamps.
- Review new source alongside tracked changes. Ignore build outputs, profiles,
  game installations and logs. No push is implied by the implementation milestones;
  the final package, gameplay/lifecycle and performance evidence remains user-owned.
