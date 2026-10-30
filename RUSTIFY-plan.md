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
integration remain intentional boundaries. Full D3D guest migration is separate work.

The app builds and signs. Existing Metal graphics suites and independent
consumers passed at the latest checkpoint; a short scripted app run exercised
settings, frame completion and clean exit. This does not establish sustained
gameplay, physical-loss recovery or comparable performance. The FPS overlay uses
GPU completion rates because wgpu supplies no drawable display timestamps.

## Upstream scope

Retain rendering profiles, world/UI/overlay composition, input coordinate mapping,
settings, capture/readback, frame pacing and existing graphics API behavior.
Preserve CPU-only/null-host operation and useful fake-backend checks. D3D11
retirement is the explicit API removal; unsupported entry points fail by name.

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

The user performs final verification after implementation is settled:

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
Linux/Vulkan, Windows/DX12 and mobile/web remain unqualified until exercised.

## Follow-on work

After this upstream checkpoint, deliver separately as needed:

- Remaining D3D7/8/9/D3DX guest migration and surface coherence improvements.
- Automatic physical-device recovery beyond the kit's existing behavior.
- Translator library reuse and Rust interpreter integration.
- Runtime memory/loading, dispatch/imports, COM and scheduler ownership changes.
- Other platform qualification and independent-consumer improvements.

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
