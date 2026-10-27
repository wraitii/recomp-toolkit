# Rust translator

`translator/` is the default engine. Its Cargo workspace translates a game image
from its code map into C. `tools/build.py` selects it unless `[translate] engine`
or `--engine` overrides the default. The generated-code contract is documented in
[architecture](architecture.md). Each crate's `//!` doc states its responsibility.

## Pipeline

| Stage | Crate | Produces |
| --- | --- | --- |
| Settings | `rk-core` | `Settings` from `recomp-translate-input-v1` |
| Load | `rk-image` | `Program`: relocated PE, code-map functions, spans, tables, classified entries |
| Discover | `rk-discover` | Provisional functions and entries the map misses, merged into `Program` |
| Lift | `rk-frontend` (`rk-sleigh`, `rk-decode`) | One `FunctionIr` per function: p-code, CFG, tables, SEH, noreturn |
| Facts | `recomp-translate` (`facts.rs`) | Analysis facts checked against the lifted code: call and jump bounds, site and import effects |
| Global | `rk-global` | Call contracts, x87 control-word modes, direct-variant targets |
| Plan | `rk-ir` → `rk-ssa` → `rk-plan` (+ `rk-x87`) | `PlannedFunction`: prepared SSA, publications, flag plan, x87 plan |
| Emit | `rk-emit` | C bodies and wrappers, or a trap body with a reason |
| Write | `rk-output` | The generated tree |

`rk-core` is the shared vocabulary. `rk-ir` is the lowering and the operation
set that SSA, planning and emission share. `rk-analysis` is a separate offline
binary that produces the facts the Facts stage reads (see below).

Plan and emit run in parallel per function, in two passes. The first plans every
function and collects return-slot proofs; the second emits each ordinary body and
its selected direct variant. Bodies are sorted by address afterwards, so output
does not depend on scheduling. Unordered hash maps are banned by `clippy.toml`.

A function that cannot be lifted, planned or emitted becomes a
`recomp_unmodelled` trap body, and the report records it. Inside an emitted body,
unsupported instructions become named runtime traps. Errors in settings, inputs,
facts, global analysis or a direct variant stop the translation.

## Inputs

`tools/build.py` resolves `game.toml` and writes `recomp/translate-input.json`
in the build root: `format`, `dir`, `exe`, `code_map`, `image_base`, `sha256`,
and the resolved `translate`, `globals`, `curated` and `aux_modules` tables.
`rk-core::settings` interprets them. `--module KEY` selects an auxiliary image
and drops the main-only options. The translator also reads:

- the executable and its `recomp-code-map-v3` directory. Both must match the
  configured SHA-256 and image base.
- `[translate] analysis_facts`, a `recomp-analysis-facts-v2` JSON produced by
  `rk-analysis`. The legacy alternative is `indirect_targets` (a
  `recomp-indirect-targets-v1` TSV) plus `call_effects`, and the two sources
  cannot be mixed. Every site is checked against the lifted code. Call and jump
  bounds feed discovery and global analysis, and effects replace the
  conservative contract at their sites.
- `[translate] discovery_evidence`, a TSV of extra discovery seeds.
- `[translate.native] header` and its includes, scanned for `FN_<addr>`
  replacements. Those functions are treated as opaque.

## Command and outputs

```sh
recomp-translate --input FILE --out DIR [--module KEY] [--report FILE] [--jobs N]
                 [--x87-mode auto|dynamic|fast] [--dump STAGE ...] [--only ADDR ...]
                 [--discovery-report FILE]
```

`build.py` builds the binary into `recomp/cargo`. It translates each image into a
staging directory (`aux-KEY/` for auxiliary images) and syncs the tree into
`recomp/gen`. The output tree contains `chunk_*.c` bodies (64 KiB address
buckets under a 4 MiB source budget), `chunk_entries_*.c` entry thunks,
`funcs.h`, `body.h`, `table.c`, `call_facts.c` and `symbols.json`. The report,
`recomp/translate[-KEY]-report.json`, gives counts, entry kinds, global-analysis
statistics, fact bounds and effects consumed, trap and fallback reasons, stack slot
promotion failures, return-slot proofs, slow operations, discovery traps, diagnostics
and phase timings. `--discovery-report`
writes the discovered code map, instruction lengths and every rejected function
or entry with its pass and reason. `--dump program|fir|ssa|prepared|stack|global|bodies|all`
prints intermediate stages to stdout, and `--only` restricts the dump to a set
of functions; `--selection FILE` also accepts a JSON `functions` array with hex
`addr` fields. `ssa` uses conservative facts without x87 recipes. `prepared` and
`stack` use production call facts, x87 recipes and preparation settings for the
ordinary body. `stack`
reports frame rejection reasons, aligned/local versus entry-frame addresses,
exposure, retained accesses, call guards and possible writes blocking local loads.
These dumps run outside `rk-plan`.

There is no emission cache or `--regen-function`. `build.py` fingerprints each
image from the input JSON, the translator binary, the code-map files and
executable, the fact, evidence and native-header files, and these environment
switches:

| Variable | Effect |
| --- | --- |
| `RK_NO_DIRECT` | No direct-variant targets |
| `RK_NO_DESCRIPTORS` | Publish flags eagerly instead of through lazy descriptors |
| `RK_NO_ROOTS` | Skip root coalescing in `rk-ssa` |
| `RK_X87_DYNAMIC` | Force dynamic x87 control-word clones, like `--x87-mode dynamic` |

## Analysis facts

`[translate] analysis_facts` names a committed, executable-hash-bound JSON file.
Translation reads it as-is. Generate a candidate explicitly from the kit root,
compare it with Git, and retain custom edits when adopting it:

```sh
python tools/build.py --game ghost-recon --produce-analysis-facts /tmp/facts.json \
    --analysis-observation /absolute/path/observe.tsv \
    --analysis-report /tmp/facts-report.json \
    --analysis-compare games/ghost-recon/metadata/analysis-facts.json
```

The build wrapper builds `rk-analysis` and runs it on `recomp/analysis-input.json`,
which adds the game's `[analysis]` table to the translation input. That table
names method and effect catalogs (both present or both absent), observation TSVs,
effect families, per-site overrides and assumptions. The producer reruns discovery
and lifting, bounds calls and jumps from tables, SSA shapes and stack-cleanup
proofs, and joins them with catalogs and observations. It validates the output
with the same `rk_core::facts` parser used by translation. `--analysis-report`
writes selection details; `--analysis-compare OLD` compares against older facts
without merging them. Committed targets and effects are never inference inputs.

With `[analysis] guest_bounds`, the producer freezes a byte-derived table census
before refining SSA and contracts. `assume_constant_vptr_stores` restricts it to
tables installed by decoded `MOV mem32, immediate32`; this assumes every runtime
vtable at a bounded site has such an installation. Otherwise all decoded immediate
and absolute-address references are candidates. Table contents are assumed immutable.
Every candidate slot must name a successfully lifted function, and every observed
guest target must be included; shim or unsupported observations block the site.
`assume_compatible_cleanup` additionally permits interface-prefix compatibility
narrowing. Cleanup summaries check ESP balance through direct calls and normal/tail
returns; unknown cleanup never rejects a candidate. Optional `assume_balanced_returns`
also uses a single byte-checked RET immediate shared by all reachable returns in a
closed single-entry function without exception flow or tail exits. This assumes ESP
is restored to the incoming return-address slot; it does not prove frame balance.
Verified and assumed summary counts are reported separately. Neither proves stack
alias or escape safety. Dominating earlier calls contribute slot and cleanup requirements
only when receiver and vptr SSA values both match. Refinement intersects proven bounds
across passes over the frozen universe, retaining prior bounds for unresolved sites;
empty intersections or exclusions of observed targets are rejected and reported.

SSA memory uses exact entry-ESP and absolute ranges plus residual memory. EBP accesses
need a proved frame derivation. Bounded addresses carry may-write footprints; unknown
reads depend on every possibly aliased range and residual memory. May-definitions retain
the prior version, and disjoint writes retain the existing range definition. Different
address roots alone never prove disjointness. When every incoming memory path establishes
the contents of an exact frame or absolute slot, forwarding builds a shared value phi
for its joins and loop backedges; unknown definitions retain the load. Guest stores remain.
Projections share by exact address, width and memory version, preserving predecessor
order. Provisional phis close cycles, but publication requires every input to succeed
and a value other than the reload itself to anchor the graph. Failed attempts discard
their provisional values without caching them; depth and memo limits retain the load.
Read/write summaries use the call-graph SCC fixed point, translate frame offsets at
each call site, and widen unresolved or
unbounded effects to opaque memory. Types and local layouts are not alias proofs.
General production call effects remain opaque. Private-frame forwarding can use interprocedural
effects, including frame-relative accesses and register/stack pointer parameters. Borrowed-stack
summaries compose over call-graph SCCs; unbounded recursive effects remain conservative.
Unknown targets, captures and unbounded parameter accesses remain conservative. Bounded
indirect target sets combine every target's effects; cross-call deferral currently requires
a direct call. General original-code summaries feed the offline inference below.

`[translate] assume` lists opt-in premises about the original code; it is empty by
default. `DIVERGENCE(original):` a listed premise is trusted, not checked, and code that
breaks it can read stale or wrong frame bytes.

| Name | Assumes | Breaks on | How to check |
| --- | --- | --- | --- |
| `private-stack` | Frame bytes whose address never escapes are reached only through ESP/EBP-derived addresses of their function, not through pre-existing or wrapped pointers. Callees reach the caller's frame only through their argument area, argument writes and escaped addresses. Hooks, replacements, observers and scheduler work at checkpoints may read but not write it. | Dangling pointers into a returned or reused frame; frame walks the escape tracking cannot see; hooks or replacements writing caller locals; one thread inspecting another's stack. | `--dump stack --only ADDR` shows exposure and rejections; translate without it and run the paired diff. `exposed_stack_functions` drops it for listed owners. |
| `stack-pointer-bases` | An escaped frame pointer is the lowest address its consumers touch: exposure is `[pointer, entry ESP)`, and base plus an unknown index stays in that range. | Callees stepping below a passed pointer: `sub ecx, N` this-adjustors for secondary bases of stack objects, member-to-container conversions, negative indices, reverse walks from a buffer end. | Look for adjustor thunks referenced as pointers and for classes with secondary bases constructed on the stack. |

Without `private-stack`, frame aliasing stays conservative. With it, explicit
ESP-derived accesses still alias normally. Load forwarding requires a single entry
and no opaque non-call effects or escaping branches. Escaped ranges retain
conservative aliasing independently of unexposed slots. Cyclic address joins require
agreeing outside inputs and zero net offset around every traversed cycle; unanchored
cycles remain unknown. Frame loads forward across proved-disjoint calls, including
aligned locals and read-only callees; other loads do not forward across calls.
Computed return values retain proved access footprints while recording their input
capture sources; passed frame pointers remain exposed conservatively when their
returned offsets are unknown. Unknown ESP-derived returns remain unsupported.
Without `stack-pointer-bases`, an escaped frame pointer exposes the whole frame.
With it, unknown upward extents expose the remainder of the frame, and proved callee
ranges can narrow exposure and include negative accesses. Each exempted write needs a
proved current ESP below the entire negative-offset slot; arguments and the return
address remain conservative. The report's `return_slot` applies the private-frame
rule to the return address alone: a function is proved when every RET reads the
slot at entry ESP and no effect may write it. Direct callers of a proved function
skip the return check; other direct calls compare EIP with the continuation and take
the general return on a mismatch.
Forwarding retains guest stores, and original-code analysis summaries do not use
this premise.
With relaxed fault state, eligible x87 accesses split into rounded value and memory
effects independently of lazy writes, preserving pop, status and ESP provenance.
Forwarding, region partitioning and dead-store elimination share one frame model
and one clobber query per effect: a contained value, transparent, or may-write.
Calls are transparent only to private-frame ranges. Loads forward contained slices
of wider values, and stores fully covered on every path by a later exact write
before any read are removed. Proved call cleanup is always used for ESP
provenance; the emitted ESP after such a call is reread from guest state.

Lazy stack writes run under `private-stack` when fault state is relaxed and store
hooks are disabled; `[translate] lazy_stack_writes = false` turns them off for
debugging. Up to 64 exact negative frame slots of 1, 2, 4 or 8 bytes become state keys: the SSA is rebuilt
with their contained loads, stores and x87 values as key reads and writes, so they
share register publication and residency. Slots are relative to entry ESP or to the
function's most used ESP alignment, `(entry ESP + c) & mask`. Calls, returns and
accesses that may touch a slot demand its publication; those that may write it reload
it afterwards, and dead-store elimination keeps their stores. Overlapping slots and
stores below ESP stay in memory. A store publishes immediately when every path demands its value before overwriting it,
otherwise at the first demand. Constant addresses never touch frame slots.
Preparation consumes checkpoint credit only when accepted;
otherwise pending slots publish before ordinary dispatch consumes its checkpoint.
Non-leaf callees require credit for the longest nested call path as well as entry;
recursion, call-containing loops and unresolved dispatch retain eager publication.
Under `private-stack`, a call whose memory summary excludes a slot does not reload it
even without checkpoint credit; credit only lets the call skip publication. A guarded
call has one inline guard, `recomp_direct_`: `recomp_slow_routes` (raised by hooks,
profiling and frame watch), the callee's `call_ready_` byte (clear under a native
replacement), checkpoint credit, and the ST0 and x87-mode conditions of a direct
variant. Its unlikely generic arm publishes the register
fields hooks observe and the slots the call does not otherwise demand, and nothing is
reloaded afterwards. `[game] call_hooks = false` removes hook publications and
kept-field rereads everywhere, and entry stubs test `recomp_slow_routes` instead of
per-function slow flags. Slots below a call's ESP are published before it and
reloaded after it. Unknown effects, alternate entries and exposed ranges keep frame
stores in memory. `DIVERGENCE(original):` eligible private-frame writes are delayed;
ordinary entry paths preserve final bytes.

`[translate] stack_memory` enables additional original-code summaries for offline
inference; it defaults to false. Ghost Recon enables it with the explicit premise
that no concurrent guest execution interferes with these calls. This setting does
not disable threads or change scheduling. The summaries model unmodified guest
code; hooks replacing that behavior must respect its contracts or use the runtime's
existing fallback paths.

The frame-balance analyzer tracks entry-ESP offsets, saved register values, stack-slot
contents and the incoming return address. Joins retain only identical values.
Every normal return must restore ESP and retain the original return-address slot;
all returns must agree on cleanup. Writes must have a proved stack address and
must not overlap that slot. Potentially aliased pointer/global writes, unknown
callees, recursive dependencies, native replacements, exception flow and unsupported
effects leave the function unknown. Direct calls and tail calls compose only
already-established summaries; indirect bounds cannot bootstrap function proofs.
Named rejection counts and frame-checked cleanup counts are reported separately.
Cleanup-only inference separately checks ESP balance over pcode, including x87,
with conservative memory and register effects for unresolved recursive candidates.
Candidates come from return encodings and tail targets; failed proofs remove them
and their dependents. Candidates start out preserving EBX, ESI, EDI and EBP and
narrow to what their proof keeps. Functions without returns, tail exits, import
tails or unresolved jumps never return and take cleanup 0 unchecked. Code-map jump
tables are followed. Calls through IAT slots, IAT thunks or registers loaded from
an IAT slot pop the runtime shim's declared arguments (`import_abi.py` reads the
shim tables into `import_pops`), keep EBX/ESI/EDI/EBP and write any memory; imports
with unknown or conflicting argument counts stay unresolved. FS-relative stores
leave stack slots intact. Under `private-stack`, other non-stack stores and
opaque callees keep slots below entry ESP. Interior entries and SEH do not block
cleanup proofs: only paths from the entry are checked, assuming MSVC `__except`
resumption restores the frame's ESP and leaves through the same returns.
Straight-line helpers that move the caller's ESP have their instructions evaluated
on the caller's state, and probes lower the caller's ESP by a constant EAX. MSVC
`__SEH_prolog`/`__SEH_epilog` and the MSVC 7 `_chkstk` are detected by their bytes;
`[translate] stack_helpers` and `stack_probes` add helpers that do not match, and
`--dump global` lists each as detected or configured. `stack_cleanup = [{ call|function, bytes, evidence }]`
states the bytes an indirect call site or a function pops; function entries are
trusted as ordinary calls preserving EBX/ESI/EDI/EBP. `evidence` records the binary
evidence and `--dump global` prints it.
SSA call summaries also consume declared cleanup at direct IAT calls, calls to IAT
thunks and configured indirect sites; `--dump global` lists these as `cleanup-site`.
Shim-table `ImportMemory` facts also reach these static import sites (`import-memory`
in the dump). `CallerStackPreserved` and `NoGuestWrites` protect unexposed caller
slots above the import's return address. Dispatch checkpoints retain unknown
shared-memory effects, even for shims marked `NoGuestWrites`.
Cleanup facts alone give no memory, return-address or checkpoint guarantees.
Proved functions are summarised again against their callees' summaries
to record constant writes at or above entry ESP (`argument-writes`). A function has
none when an address at or above its entry ESP is stored, passed in a register,
returned, or indexed for a store, or when it has SEH or interior entries.
Under `private-stack`, a call clobbers only the caller slots its argument writes
cover. `--dump global` lists per-function rejections, unresolved call edges, cleanup-only proofs and argument
writes.

At a bounded call, every returning candidate must have a summary with matching
cleanup. Mapped noreturn candidates remain dispatch targets but contribute no
normal-continuation effects; this trusts the existing code map's noreturn facts,
not a newly inferred type or an immutable-global claim. Read footprints do not
establish argument counts, object sizes or compatibility. Unknown remains compatible.
Inference may retain a load across a call only when its range is proved disjoint
from every candidate write. CALL's explicit return-address store remains in the
memory chain and undergoes the same alias check; different address roots alone
never establish disjointness. The additional continuation facts are confined to
offline inference; runtime body preparation keeps its existing call effects.

`reuse_discovery_seeds` permits configured `analysis_facts` targets to become
byte-validated discovery seeds only. `indirect_targets` and `discovery_evidence` are
cleared, the normalized unique `(site, kind, target)` seed rows are digested, and the
seed universe remains distinct from proof. Clean runs clear `analysis_facts` too.
Discovery must converge before the producer emits bounds, and the stable function
universe digest is reported. Refinement freezes the discovered program and raw table
census; later contract passes may improve site shapes but cannot add tables.
Ghost Recon enables `guest_bounds`, `assume_constant_vptr_stores`,
`assume_compatible_cleanup` and `assume_balanced_returns`; discovery seed reuse is off.

The report separates discovery mode and seed provenance, raw census and install
counts, summary/blocked-function and shape-rejection counts, open-site reasons with
examples, per-pass contract/CW coverage, convergence, and target-set comparison as
same, narrower, wider, incomparable, missing or added independently of effects.

`[analysis] families` selects broad patterns such as `d3d8.dll!*::*`.
`assert_effect_coverage = true` permits assuming that an observed method's audited
state effects cover all executions of its site. False emits no such effects.
`[[analysis.overrides]]` can select `family`, `interface`, `method`, and/or an eight-digit
hex `site`, and set `enabled` or a named catalog `effect` with `evidence`. Selectors
combine; site, method, interface, then more-specific family rules take precedence,
with the later rule breaking ties. The winning rule uses `assert_effect_coverage`
when `enabled` is omitted and the catalog when `effect` is omitted.
CLI observation paths replace configured `observations`; config paths are relative to
the game directory. Mixed, unidentified, or conflicting observations remain unresolved.
Disabling a policy affects the next explicit generation, not existing committed facts.

V2 retains V1 shared `targets`, `effects`, `sites`, `imports` and `evidence`, and adds
symbolic `methods` (site key `m`), assumption provenance and input digests. `regs` accepts
`abi`, `unknown`, or explicit `reads`/`kills`/`writes`/`preserves` lists containing GPR and
arithmetic-flag names. CW effects use `preserved`/`known`/`value` masks. Method identities
are not exhaustive target bounds. A call site's `prefer_guest: true` selects the
guest-preferring dispatch helper when all supplied observations reach guest code,
including auxiliary modules. Mixed or unknown observations leave the site generic.
The preference requires no vtable shape or target bound and grants no register,
stack or effect guarantees; unresolved targets retain the helper's normal slow
fallback. Only exhaustive guest target sets feed dispatch
bounds; sites are instruction addresses, `k` defaults to `call`, and `jump` means a tail
JMP. Explicit unknown effects stay conservative. Rust recomputes function preservation
and CW summaries. V1 remains supported; legacy `indirect_targets` and `call_effects`
cannot be combined with `analysis_facts`.

The shared D3D8 slot catalog is generated by `tools/gen_com_interfaces.py` with
`--reference third_party/wine-d3d8/d3d8.h --bindings dx/d3d8_bindings.json --dll d3d8.dll
--catalog-output dx/metadata/com-methods.json` (`--check` checks freshness). Audited
shim effects live in `dx/metadata/method-effects.json`; the current D3D8 handlers
preserve guest arithmetic flags and GPRs other than EAX/ESP. Reads remain conservative.
CreateDevice preserves only the CW bits outside `0x0f3f`: its preserve flag and early
returns prevent a stronger unconditional result. Other D3D8 methods carry no CW
effect: generated code does not consume their preserve bits, and the optional
call-fact check treats an absent effect as exact equality. Callback-capable or unaudited methods
need conservative effects until their complete behavior is established.

## Direct variants

`[translate] direct_variants = true` selects closed, returning leaves with at least
two direct call sites and at most 64 original instructions per site. Opaque
observers, native replacements and unsupported contracts exclude targets.
`direct_arguments = { "ADDR" = ["ECX", "ST0"] }` explicitly selects a function
and passes its listed GPR inputs and optional ST0 as C arguments; unused GPRs are
removed by the callee's read contract. Auxiliary modules use ordinary bodies.

Each selected function has one `direct_ADDR` body, determined by its C arguments
and x87 mode. It publishes the full return state, with the same SSA publication
and residency reductions as the ordinary body. Callers retain their own live
reloads. No output masks or per-mask entry thunks remain. The inline
`recomp_direct_` guard calls the body directly; native replacement edits change
only the stable entries and their `call_ready_` bytes. Hooks, profiling, exhausted
checkpoint credit and incompatible ST0/x87 modes use `call_ADDR` instead. Both
arms share the caller's return policy. `RK_NO_DIRECT=1` disables variant selection;
disable `direct_variants` and clear `direct_arguments` to do so in configuration.

## Open work

Guest state is converging on one model: register lanes, flag fields and promoted
frame slots each have an SSA value and a guest home; boundaries demand, clobber or
pass through each location; a guarded boundary has one route condition with a fast
arm under its contract and a slow arm that publishes and reloads everything the
generic path may observe. Remaining steps:

- One placement pass over registers, flags and slots (anticipated demand plus
  availability, cold arms preferred), replacing residency publication and the eager
  slot heuristics. Reloads stay at the call unless dead on some paths.
- Refuse promotion when a may-clobber inside a loop forces per-iteration reloads of
  a slot the loop reads.
- Cleanup roots behind indirect calls, such as RT3's `005193f0` and `00518de0`.
- A shared SCC solver for the `rk-global` stack, memory and borrowed-stack analyses.
- Address-list hook sets, so only hookable targets keep the hook route.
- Cheaper `recomp_direct_` routes: leaf callees that cannot yield need no
  checkpoint decrement.
- Narrow `stack-pointer-bases` with facts: a frame pointer pushed as an argument
  of a callee whose borrowed-stack summary bounds that argument needs no premise.
  Imports alone cover few cases (RT3: 36 of the 594 bodies the premise changes).
- Released-stack retirement, removed because its proof rejected any read before the
  next push or call and contract routes took precedence: it needs a proof that the
  callee's frame does not escape, a continuation check that rejects only reads that
  may alias released bytes, and a variant for retired frames.
- Stack arguments and x87 spill slots as plain values, leaf functions without ESP
  materialization, and direct return values passed by value.
- Return-slot coverage: preserve ESP provenance across unproven call cleanup and
  model DIV/IDIV memory effects precisely.
