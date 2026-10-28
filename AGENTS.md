# Working on recomp-kit

The kit is the translator, guest runtime, DirectX/Win32 shims and native hosts.
Supported games live in `games/<id>/` (config, code map, native replacements);
installations and builds stay in the ignored build root, `build/<id>/` by
default. `docs/code-guide.md` maps behavior to source; `docs/architecture.md`
explains guest memory, threads and frames.

## Ground rules

- Do not write tests unless asked. Check translator changes with the translator cases, then the
  diff harness, below.
- Avoid comments; code should read clearly without them.
- Before adding code or documentation, check whether it can be shorter or folded
  into an existing section.
- The repository has legacy; existing patterns are not authoritative.

## Invariants

- Guest addresses are 32-bit values accessed through the memory helpers, never
  host pointers. Guest memory changes only on the scheduler baton holder; the
  presenter only sees sealed frames.
- Nothing in `runtime/`, `dx/`, `host/`, `platform/` or `mods/` names a game
  (`tools/check_game_literals.py`). Game addresses go in `game.toml` and code
  reaches them through generated `RECOMP_HOOK_*` macros.
- Operating-system calls go through `platform/os.h`; platform `#ifdef`s live only
  in `os_posix.cpp` and `os_win32.cpp`.
- Never edit generated code under `build/recomp/gen/`. Fix the translator and
  rebuild with `--regenerate`.
- Unknown imports, COM methods and unsupported state fail with a named
  diagnostic rather than a silent stub.
- Build only through `tools/build.py`. Format native code with
  `tools/format.py --write`; leave `third_party/` as vendored.
- Never commit game files, generated code, binaries or logs.

## Translator cases

`tools/test.py --translator [--case NAME]` assembles `tests/translator/cases/*.s` into a
synthetic image, translates it with the current translator, and compares every case
function with Unicorn on 200 inputs. It needs no game and runs in seconds; run it first
after any translator change. Case conventions are in `tests/translator/run.py`.

## Diff harness

`tools/recomp/diff/run.py --game <id>` runs translated functions from the
last build and the original bytes under Unicorn on the same random registers,
arguments, scratch memory and uninitialized globals, then compares registers,
flags, x87 state and writable memory. Inputs where the original faults, calls an
import or runs too long are discarded. Select functions with `--func ADDR`,
`--sample N`, `--all`, `--changed`, or `--suite MANIFEST`.
The default is 25 random inputs followed by five structured seeds with zero or
uniform memory and simple pointers. `--iterations N` changes the random count;
`--no-structured-inputs` restores random-only runs. Suites keep their recorded schedule.

`--mode specialized` (default) runs the production contract call sites,
`--mode ordinary` forces the ordinary fallback through `recomp_hooks_ever`, and
`--mode paired` runs both on each input, comparing each with the original and
the two with each other. The same linked library serves both modes, so
`checkpoint_countdown` is seeded nonzero for private routes to run. There is no
per-site instrumentation: paired equality is necessary, not proof that a call
site took the private route.

Each run writes a fresh report directory and its own harness; `--out` refuses
reuse. `--changed --baseline FILE` reads a chosen body snapshot, while
`--save-baseline FILE` creates a new one exclusively. `--fail-on any` (default)
fails on oracle or pair discrepancies; `--fail-on pair` requires paired mode.
Run provenance and replay limits are recorded in `run.json`; see
[testing](docs/testing.md) for artifact and input details.

For a translator change, after the translator cases pass, run at most 200-300 functions;
`--all` is slow. Snapshot bodies with `--sample 1 --save-baseline before.json` (the baseline
covers every body), change and `--regenerate`, then `--changed --baseline before.json` when
few bodies changed, or `--sample 300` otherwise (add `--mode paired`).
Inputs reset writable sections, BSS/scratch/TEB fills and the full stack window
before the original and every variant, without redrawing random values.

The harness is compiled only through `tools/build.py --diff-harness`, which links
the existing generated archive whole without rebuilding or regenerating it.
`--fpu-cw` fixes the initial x87 control word; divergences only under `007f`
usually come from binary32 x87 arithmetic. Only the active ST0 and TOP are
compared, using the tag word to tell active from empty and ignoring inactive
scratch slots; ST1-ST7 and the full status word are out of scope.
