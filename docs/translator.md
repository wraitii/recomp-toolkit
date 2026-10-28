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
| Global | `rk-global` | Call contracts, x87 control-word modes, private-contract targets |
| Plan | `rk-ir` → `rk-ssa` → `rk-plan` (+ `rk-x87`) | `PlannedFunction`: prepared SSA, publications, flag plan, x87 plan |
| Emit | `rk-emit` | C bodies and wrappers, or a trap body with a reason |
| Write | `rk-output` | The generated tree |

`rk-core` is the shared vocabulary. `rk-ir` is the lowering and the operation
set that SSA, planning and emission share. `rk-analysis` is a separate offline
binary that produces the facts the Facts stage reads (see below).

Plan and emit run in parallel per function, in two passes. The first pass emits
every function that neither calls a private-contract target nor is one. It
collects the field masks those calls need, `rk_global::specialize` merges them
into variant layouts, and the second pass emits the remaining functions and
their private variants. Bodies are sorted by address afterwards, so output does
not depend on scheduling. Unordered hash maps are banned by `clippy.toml`.

A function that cannot be lifted, planned or emitted becomes a
`recomp_unmodelled` trap body, and the report records it. Inside an emitted body,
unsupported instructions become named runtime traps. Errors in settings, inputs,
facts, global analysis or a private variant stop the translation.

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
statistics, fact bounds and effects consumed, trap and fallback reasons, slow
operations, discovery traps, diagnostics and phase timings. `--discovery-report`
writes the discovered code map, instruction lengths and every rejected function
or entry with its pass and reason. `--dump program|fir|ssa|prepared|global|bodies|all`
prints intermediate stages to stdout, and `--only` restricts the dump to a set
of functions. The `ssa` and `prepared` dumps build SSA outside `rk-plan` and are
diagnostic only.

There is no emission cache or `--regen-function`. `build.py` fingerprints each
image from the input JSON, the translator binary, the code-map files and
executable, the fact, evidence and native-header files, and these environment
switches:

| Variable | Effect |
| --- | --- |
| `RK_NO_PRIVATE` | No private-contract targets |
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
address roots alone never prove disjointness. Read/write summaries use the call-graph
SCC fixed point, translate frame offsets at each call site, and widen unresolved or
unbounded effects to opaque memory. Types and local layouts are not alias proofs.
Production call dispatch remains opaque to memory forwarding because hooks and scheduler
handoffs can add effects; original-code summaries feed the explicitly enabled offline
inference below.

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
