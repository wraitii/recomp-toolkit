# `runtime` — guest runtime, image loader and Win32 shims

This directory is the host side of the static recompilation: it owns the guest
address space, loads the game's executable into it, and implements the Win32
imports the game calls. It knows nothing about game logic; every shim
implements a Win32 contract and nothing else.

The CPU state (`struct X86`), the memory accessors (`rd32`/`wr32` and friends)
and `recomp_call` come from `runtime/x86.h`, which the translator
owns. Compile the runtime with `-I<repo root>` so `guest.h` can find it.

## Files

| file | contents |
| --- | --- |
| `guest.h` | address-space constants, guest string helpers, logging, `guest_call` |
| `memory.h/.cpp` | the 256 MB arena and the first-fit heap allocator |
| `loader.h/.cpp` | SHA-256 check, PE mapping, IAT patching, TEB/stack setup, `run_entry` |
| `imports.h/.cpp` | shim table, trampoline allocation, dispatch, coverage reporting |
| `cpu.cpp`, `intrinsics.h` | the call-outs `x86.h` declares and the `_setjmp`/`_longjmp` intrinsic contract |
| `kernel32.cpp`, `kernel32_wide.cpp` | heap, files, modules, TLS, sync objects, time, strings, locale and the UTF-16/INI entry points |
| `user32*.cpp` | window classes, windows, messages, cursor/dialog/static/VCL helpers and UTF-16 entry points |
| `gdi32*.cpp` | the GDI model: objects, DCs and palettes; software draw, text and TrueType rasterization |
| `misc.cpp`, `misc_dlls.cpp` | ADVAPI32 registry, SHELL32, ole32, IMM32, WSOCK32, WINMM, and small optional-DLL probes |
| `comctl32.cpp`, `imagehlp.cpp`, `media_foundation.cpp`, `msimg32.cpp`, `oleaut32.cpp` | common controls, DbgHelp, Media Foundation, msimg32 and automation shims |
| `seh.cpp`, `thunks.cpp` | guest exception checkpoints and thunk dispatch |
| `interp.cpp`, `pcode.cpp`, `pcode_x86.cpp` | fallback selection, p-code evaluation and x86 state/dispatch; [behavior and build options](../docs/architecture.md#discovery-and-code-a-build-does-not-carry) |
| `call_trace.cpp`, `linear_trace.cpp`, `discovery.cpp`, `observe.cpp`, `profile.cpp` | call tracing, discovery and profiling diagnostics |
| `installation_manifest.cpp`, `layout.cpp`, `key_names.cpp`, `resources.cpp` | installation, input layout, key names and resource helpers |
| `mods_seam.cpp`, `native_seam.h` | the mod and native-override seams |
| `win32.h` | what the host app and the DirectX shims call into |
| `tests/` | `runtime_tests.cpp` plus the test-only `recomp_call` |

## Address space

Guest address `a` is `g_mem + a`.

| range | contents |
| --- | --- |
| `0x00400000`–`loader_image_limit()` | image at its preferred base, no relocation (the limit comes from the image's `SizeOfImage`) |
| `[game] heap_base`–`heap_end` (default `0x01000000`–`0x0e000000`) | heap arena (HeapAlloc/GlobalAlloc/LocalAlloc/VirtualAlloc) |
| `0x0e800000`–`0x0f000000` | stack, growing down |
| `0x0f000000`–`0x0fd00000` | mod heap |
| `0x0fe00000` | TEB: FS:[0] SEH head, FS:[4]/[8] stack bounds, FS:[0x18] self, FS:[0x2c] TLS |
| `0x0fe01000` | 64 TLS slots |
| `0x0ff00000 + 16*i` | trampoline for import `i` |

Handles are small integers outside the arena's mapped regions and are never
dereferenced: kernel objects from `0x00010004`, windows from `0x00020004`,
icons from `0x0002a000`/`0x0002b000`, registry keys from `0x00030004`, mmio
files from `0x00040004`, stock GDI objects from `0x0004f100`, bitmaps from
`0x00050000`, palettes from `0x00058000`, DCs from `0x00060000`, image icons
from `0x00090000`, pseudo modules from `0x60000000`.

`loader_init_context` sets the process-start CPU state: x87 control word
`0x037f`, clear status word, all-empty tag word `0xffff`, `fpu_top` 0, and
`eflags_misc` `0x202` so `PUSHFD` reads `0x202`. The CRT's `__ftol`,
`__ctrlfp` and `__statfp` depend on those x87 values.

`CreateProcessA` supports restarting the translated image through the windowed
host, preserving the installation, inherited profile environment and guest
arguments (`--guest-args`). Other executables, custom environments, working
directories, security attributes and startup/creation flags fail explicitly.
Returned process handles support close, waits and exit-code queries. Child
primary-thread handles can be closed; status/waits on POSIX fail with named
diagnostics because host process exit does not establish guest thread exit.
Restart children wait up to 30 seconds for the previous instance's profile lock;
ordinary launches still reject an occupied profile immediately.

## The shim contract

```c
struct ImportShim {
    const char* dll;
    const char* name;
    uint8_t     argc_stdcall;   // dwords the shim pops; ARGC_CDECL, ARGC_UNKNOWN
    void      (*fn)(X86*);      // nullptr => logging-only, returns EAX = 0
    ImportMemory memory = ImportMemory::Unknown;
};
```

On entry to `fn`, ESP points at the guest return address and argument `i` is
`arg(c, i)` (the dword at `ESP + 4 + 4*i`). A shim writes its result with
`set_eax` (or `set_eax64` for a 64-bit result in EDX:EAX) and returns. It never
touches ESP: `imports_dispatch` pops the return address plus `4*argc_stdcall`
bytes and sets EIP, exactly as the real `ret n` would.

Optional memory facts describe the shim: `CallerStackPreserved` forbids writes
above its entry ESP, including writes through output pointers or callbacks;
`NoGuestWrites` forbids all guest-memory writes. Both default to `Unknown` and
are extracted with stack cleanup by `tools/import_abi.py`. Dispatch can schedule
other guest threads, so these facts preserve only unexposed caller stack slots;
they do not make the whole import call free of shared-memory effects.

`ARGC_CDECL` pops only the return address. `ARGC_UNKNOWN` pops only the return
address too, but logs loudly the first time, because leaving a stdcall callee's
arguments on the stack makes the guest stack drift.

To call back into the guest — a WNDPROC, a thread body, a multimedia timer
callback — use `guest_call(c, fn_addr, a0, a1, ...)`. It pushes the arguments
right to left plus a sentinel return address, dispatches through `recomp_call`
and restores ESP.

Adding a shim means adding one row to the table at the bottom of the relevant
`.cpp`. Registration happens once in `imports_init()`. The DirectX adapter adds
COM vtable slots to the same trampoline range with:

```c
uint32_t imports_alloc_trampoline("ddraw.dll", "IDirectDraw::Blt", fn, argc);
```

Anything the loader finds in the IAT that has no registered shim still gets a
trampoline, so a call logs `DLL!name` once and returns 0 rather than jumping
into nothing. A `thiscall` method reads `this` from `c->r[R_ECX]` and its
stack arguments with `arg` as usual.

Four weanetr imports are variables rather than functions
(`BFAID_INet`, `BFAID_MODEM`, `BFSPGUID_MODEM`, `options_to_parity_table`).
Those IAT slots hold the address of zeroed guest storage instead of a
trampoline; `imports_register_data(dll, name, size)` adds more.

## `_setjmp` / `_longjmp`

The translator substitutes these by address.
Prefer the two-call form, which puts the host `setjmp` in the generated
function's own frame:

```c
{ jmp_buf *b = recomp_setjmp_prepare(c); recomp_setjmp_return(c, setjmp(*b)); }
```

`recomp_setjmp(c)` exists only to catch a call site that was not translated
into that form. It cannot work, because the host `setjmp` would belong to a
frame that has returned by the time `_longjmp` fires, so it logs what the call
site should emit and aborts.

`RaiseException` and `RtlUnwind` go through the guest exception checkpoints in
`seh.cpp`. An exception the guest does not handle logs its state and aborts;
continuing would run the guest past a point Windows never reaches.

## Behaviour worth knowing before you debug something

- **The image hash is a hard gate.** `loader_load` refuses any EXE whose
  SHA-256 is not the expected one, with no override, because everything after
  it trusts that exact layout. PE parsing bounds-checks the headers, the
  section table and every import RVA against the image.
- **`GetMessageA` blocks, and owns the wait.** It drives the multimedia
  timers, calls the host's pump, and yields the scheduler baton, round and
  round until a message matching the filter arrives. The host's pump services
  its event loop once and reports whether anything is queued; it must not wait
  itself, because only `GetMessageA` knows the filter and only the runtime can
  yield the baton. With no host installed at all it returns -1, the documented
  error, rather than blocking forever or inventing a `WM_NULL`.
- **DLL identity precedes basename lookup.** `LoadLibraryA/W` verify requested
  files against the mapped translation's recorded SHA-256 before returning a
  cached handle or attaching code. An existing unsupported DLL stops with its
  requested path and hash; a missing file returns `ERROR_MOD_NOT_FOUND`, allowing
  the original fallback. Registered shim modules remain virtual. Full-path
  handle queries and data-file loads also respect the requested file.
- **`CreateWindowExA` sends `WM_NCCREATE` then `WM_CREATE`** and honours their
  failure returns by cancelling creation, as Windows does.
- **Painting follows the window's update region, not the clock.** Showing a
  hidden window invalidates it and calls the host's window-shown hook, which is
  the transition a window manager reacts to. `UpdateWindow` sends `WM_PAINT`
  synchronously, and only when the region is dirty; `BeginPaint` validates it.
  A posted `WM_PAINT` would arrive whenever the guest next pumped, and a guest
  that never pumps would never paint.
- **Resource modeless dialogs use guest callbacks and GDI composition.**
  Screen DC pixels supply the startup background beneath child controls until
  the game renderer takes over. The SDL host presents startup dialogs in a
  separate window sized to their guest rectangle, with borders selected from
  the guest style; gameplay retains its own window and fullscreen setting. `DIVERGENCE(original):` dialog fonts use the
  existing bundled substitutes when unavailable, and progress controls use
  unthemed software painting. Modal indirect dialogs remain unsupported.
- **A wait yields to other guest threads.** Finite waits expire only after their
  deadline, and infinite waits remain pending until satisfied or shutdown. See
  **Threads** below for ownership, suspension and deadlock diagnostics.
- **`VirtualAlloc` works in whole pages**, base and size, and `MEM_DECOMMIT`
  discards the contents so a recommit sees zeroes.
- **A message box is presented by the host on its UI thread.** The guest
  caller is suspended in the scheduler while the host shows the SDL box, so
  other guest threads and host events keep running. The result is a copied
  button ID, with `MB_DEFBUTTON*` choosing the return-key default and the
  Cancel/OK button choosing Escape. Without a host presenter, or for an
  unsupported flag, the call reports a named diagnostic and returns zero.
  `DIVERGENCE(original):` SDL substitutes its information icon for the Windows
  question icon; guest UI-thread message processing pauses during native UI.

## Threads

`CreateThread` starts a **cooperative** thread and returns, the way Windows
does. The game needs that: both DirectInput service threads tell their creator
they are ready by setting a field it polls and then loop on
`WaitForSingleObject` for the rest of the process, so a body run to completion
inside `CreateThread` never comes back.

Each guest thread owns a register file, a 1 MB guest stack, a TEB and a TLS
array, and runs on its own host thread so guest code can nest host C frames
freely. **Exactly one of them executes at a time**, named by a baton held
under a mutex. Nothing in the guest arena, the shims or the generated code
needs a lock, because there is never more than one guest instruction in
flight — the same invariant the single-threaded runtime had.

**Every call into the runtime is a scheduling checkpoint.** `imports_dispatch`
yields if the running thread has held the baton for a millisecond. Scheduling
must not depend on which API the guest happens to poll: the guest's frame limiter spins
on `GetTickCount`, and when only blocking calls yielded it could starve the
DirectInput workers indefinitely with their 200 ms waits already expired. The
millisecond rate limit bounds the yield frequency. Each slice starts when its
thread acquires the baton; time spent waiting does not count toward that slice.
Translated direct, indirect, tail and auxiliary-module calls also checkpoint,
with a per-thread 1024-call budget amortizing clock and registry work. This lets
a guest-only polling loop wait for a worker without making an OS import. The checkpoint preserves the
caller's CPU and honors `sched_atomic_enter` spans. A loop entirely within one
translated body that makes no calls remains outside this call-boundary seam.

**A thread that cannot proceed blocks.** It leaves the runnable set with a
deadline, the scheduler runs everything else, and when nothing is runnable it
sleeps until the earliest deadline instead of spinning. `Sleep(ms)` suspends
the caller for the interval; `Sleep(0)` yields, as documented.

**Nothing is released by a timeout that did not happen.** A zero timeout
answers from the current state. A finite timeout expires only once the time has
passed. An `INFINITE` wait with no runnable peer and no pending deadline is a
deadlock, and it is reported as one — with the last sixteen baton hand-offs and
every thread's state — rather than answered with `WAIT_TIMEOUT`. The host's own
run deadline ends the process in that case.

**A wait is taken when it is granted.** The scheduler decrements the semaphore,
clears the auto-reset event or takes the mutex at the moment it releases a
waiter, so one signal releases exactly one of them.

**APCs are queued per thread and delivered at an alertable wait.**
`QueueUserAPC` appends the routine and wakes an alertable waiter;
`WaitForSingleObjectEx` and `SleepEx` run queued routines and report
`WAIT_IO_COMPLETION`. Non-alertable waits leave them queued.

**Suspension is a count.** `SuspendThread` and `ResumeThread` both return the
previous count and a thread runs again only at zero, so two suspends need two
resumes. `CREATE_SUSPENDED` starts it at one. Suspending yourself deschedules
before the call returns.

**Locks have owners.** A `CRITICAL_SECTION` keeps `LockCount`,
`RecursionCount` and `OwningThread` where Windows keeps them; its owner
recurses and anybody else blocks. A mutex is owned recursively by one thread,
and `ReleaseMutex` from a non-owner fails with `ERROR_NOT_OWNER`. This is not
bookkeeping for its own sake: a yield inside a protected section now really can
let another thread reach the same section.

A thread that has not finished reports `STILL_ACTIVE` from
`GetExitCodeThread`, which is itself a scheduling point because a caller
polling it is spinning, and leaves a wait on its handle unsatisfied. `ExitThread` ends its own thread. `ExitProcess` called from
a guest thread cannot longjmp, because the landing pad is on the main thread's
stack: it records the request, hands the baton to the main thread and ends
itself, and the main thread performs the exit as soon as it is running again.

## The guest file system

The guest root from `game.toml` (`executable.guest_root`) maps to the
directory holding the loaded EXE. Path resolution converts `\\` to `/`,
resolves `.`/`..`, and matches each component case-insensitively against the
real directory (cached per directory, invalidated on create/delete/rename).
Relative paths resolve against the guest current directory, which starts at
the configured guest root.

## Environment variables

| variable | effect |
| --- | --- |
| `RECOMP_LOG` | `0` silent, `1` warnings and one-shot notices (default), `2` traces every import call and returned EAX |
| `RECOMP_REGISTRY` | registry backing file (default `build/recomp/registry.json`) |
| `RECOMP_CREATETHREAD=skip` | record `CreateThread` without running the thread body |
| `RECOMP_CREATETHREAD=sync` | run a thread body to completion inside `CreateThread` (pre-scheduler behaviour, for bisection) |
| `RECOMP_IMPORT_STATS=1` | at exit, classify every import as called / not reached / logging-only |

## Tests

```
tools/.venv/bin/python tools/test.py --native          # build and run
tools/.venv/bin/python tools/test.py --compile-only    # build only
tools/.venv/bin/ctest --preset macos -R runtime_tests  # one suite
```

Run from the repository root. The tests load the real EXE, cross-check the
section mapping against `pefile`, exercise the allocator, open
`data/VCONFIG0.DAT` through a mixed-case guest path, round-trip the registry
and check that every shim leaves ESP balanced.

## MIDI out

`midiOutGetDevCapsA` reports one device called "SoundFont Synth".
`host_midi_open/short/sysex/reset/close` in `win32.h` are the host contract;
the weak defaults in `misc.cpp` accept every message and play nothing.
The host builds the TinySoundFont synth and loads the bank during startup,
before any guest thread exists, so `host_midi_open` publishes what already
exists and parses nothing. A host without a synth remains silent.

Every one of these runs on a guest thread and holds the scheduler baton for the
whole call, so a host that blocks inside one stops every guest thread and not
just the caller. `host_midi_short` is the hot one, called per note, and must
not take a lock an audio thread can hold. Because the bank is already loaded,
`host_midi_open` is timed against a two-millisecond budget: work that would
make it block belongs in `host_midi_startup`.

### Unsupported import diagnostics

Explicit unsupported handlers print the import name, guest registers, return
address, and every declared stdcall stack argument word before aborting, even
with `RECOMP_LOG=0`. Values are raw hexadecimal words, not inferred parameter
types; COM stack counts include `this`. Cdecl and unknown signatures have no
known argument count, so they show a labelled eight-word raw stack preview.
Out-of-arena words are marked unreadable; pointer arguments are not dereferenced.
The diagnostic is flushed to stderr and can be saved with `2>imports.log`.
Legacy null handlers also dump this information with their first enabled warning;
their existing zero-return behavior is unchanged.

A larger heap can use `[game] guest_size`, `heap_base` and `heap_end`. Keep
the heap wholly below `0x0e000000` or above `0x10000000`, clear of the fixed
mod, stack, TEB and import regions and any auxiliary modules. Guest addresses
remain 32-bit. Increasing `guest_size` alone does not enlarge the heap.

`FormatMessageA` supports a limited English system-message catalog, caller buffers
and `FORMAT_MESSAGE_ALLOCATE_BUFFER` with `LocalFree` ownership.
DIVERGENCE(original): neutral/default language requests use English; other
languages and missing catalog entries return the corresponding Win32 lookup
errors. Source/module messages, insert formatting and width options remain named
unsupported-import failures. `FormatMessageW` retains its existing approximation.

`GetFileTime` supports optional access/write timestamps; creation-time requests
remain named unsupported failures because the portable stat seam has no birth
time. DIVERGENCE(original): timestamps retain only whole-second precision.
`ImmIsUIMessageA` classifies IME UI messages even with a null window and forwards
classified messages through `SendMessageA` when a window is supplied.

CoCreateGuid uses a one-argument stdcall shim and returns a host-entropy
version-4 GUID in guest memory, without advancing the guest RNG.
MoveFileA/W share UTF-8 path resolution and reject an existing destination.
DIVERGENCE(original): the rename seam currently cannot move files across
volumes and checks destination existence before renaming rather than atomically.

CPUID reports FPU, TSC, CMOV, MMX, FXSR and SSE for the emulated Pentium III;
`IsProcessorFeaturePresent` agrees for MMX and SSE. SSE2 and 3DNow remain
unadvertised. MXCSR is guest state: SSE arithmetic honours FTZ, and LDMXCSR
stops with a diagnostic on any other rounding mode, DAZ or an unmasked exception.
