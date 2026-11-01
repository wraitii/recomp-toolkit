# Runtime plan

Clean up the guest runtime so that its Windows, x86, PE and generic parts are
separable, then use that to add 64-bit Windows executables. Other guests
(macOS, other architectures) are a later possibility; the cleanup should make
them cheaper but does not target them.

The cleanup phase needs no design decisions and is useful on its own. The
64-bit phase starts only after it.

## Current layering

Inventory from 2026-11-01. "Windows" means the emulated guest OS; the host OS
is already isolated in `platform/os_*.cpp`.

| Layer | Where |
| --- | --- |
| Windows (and 32-bit) | Win32 shims (`kernel32*`, `user32*`, `gdi32*`, `misc*`, `comctl32`, `oleaut32`, `imagehlp`, `msimg32`, `media_foundation`, `msvcr90`, `steam_api`); FS:[0] SEH in `seh.cpp`; MSVC `_setjmp` in `intrinsics.h`; DIK key names; TEB/stack/DLL attach in `loader.cpp`; `GUEST_TEB_BASE` in `x86.h`; handle ranges |
| x86, not Windows | `x86.h`, `pcode_x86.cpp`, `pcode_x87.cpp`, `thunks.cpp`, `reloc.cpp`, `cpu.cpp`, call/linear trace; the stdcall/cdecl handling in `imports.cpp` |
| PE, not Windows | PE mapping, IAT, relocations, exports and forwarders in `loader.cpp`; `resources.cpp`; IAT trampolines in `imports.cpp` |
| Generic | `memory.cpp`, `pcode.cpp`/`pcode_core.h`, `dispatch.cpp`, `discovery`, `profile`, `jit.cpp`, `layout`, `installation_manifest`, the seams |
| Generic but trapped in `kernel32.cpp` | cooperative scheduler (~2500–3400); guest file system (~330–610 plus listing cache and write promotion) |

32-bit is pervasive rather than a layer: `uint32_t` guest addresses, the fixed
memory map below `0x10000000`, trampolines at `0x0ff00000`, 4-byte stack
slots, and literal struct offsets in shims.

Windows semantics in `host/`: `sdl/main.cpp` and `input_gate.cpp` post `WM_*`
messages to an `hwnd`; `input.cpp`/`input_gate.cpp` produce DIK/VK codes;
`d3d_render.cpp` implements D3D render states; `boot.cpp`,
`headless_main.cpp` and `smoke_main.cpp` call the loader and `win32_*`
directly. GPU, compositor, presenter, audio, controls and launcher are generic.

### Guest file system today

1. `normalise_components` (`kernel32.cpp:344`) applies Windows path rules:
   drive and `\`, `.`/`..`, guest current directory, stripping
   `RECOMP_GUEST_ROOT` (including multi-component roots and `..` out of it).
2. `win32_host_path_op` resolves, in order: read-only aliases, the resolver
   installed by `win32_set_file_ops` (`mods/overlay.cpp`: profile, then mods in
   reverse load order), then a case-insensitive per-component match in the game
   directory. A write the resolver does not answer is refused, never sent to
   the game directory.
3. `native_seam.h` exposes readable/writable paths, aliases and temp files.

## Phase 1: cleanup

Translator untouched. Verify with `tools/test.py --native` and a game run.

### 1.1 Rule violations and leftovers

Of the 89 `nullptr` shim rows, about 64 only declare the ABI for DLLs that
`dx/` implements (QMIXER, weanetr, d3dx9 math, DirectDraw/DirectInput/D3D9
creation, DSOUND ordinals) and are overridden at registration. About 25 are
real logging-only stubs returning 0. Some games depend on calls like these
failing quietly, so none may start aborting; per-game `strict_imports` stays
the opt-in for stricter behaviour.

- [ ] Mark the ~64 overridden rows as declared-ABI placeholders so coverage
      reports stop counting them as stubs.
- [x] ~~Give each real stub a real shim, a declined shim or
      `imports_unsupported`.~~ Done; `GetDesktopWindow` was already
      implemented in `user32_vcl.cpp`.
      - Real: `CreateFontA`, `GetDateFormatA`, `GetLongPathNameA`,
        `ExpandEnvironmentStringsW`, `GetDiskFreeSpaceExA`, `HeapValidate`,
        `FatalAppExitA`, `SearchPathW`, both time-zone conversions (an explicit
        zone with daylight rules is declined).
      - Declined (`imports_decline`, documented failure value, logs once):
        `CombineRgn`, `TerminateThread`, `OpenProcess`, `SetFileTime`,
        `VerLanguageNameW`, `GetScrollBarInfo`, `SendInput`,
        `URLDownloadToFileW`, `aux*`, `Netbios`.
      - Unsupported: `DebugBreak`, `GetOverlappedResult`.
- [ ] `ReadFile` ignores its OVERLAPPED argument: no `Offset`, no byte count.
      Needed before `GetOverlappedResult` can be real.
- [ ] Resolve the 24 `ARGC_UNKNOWN` entries.
- [ ] Remove the game address from `intrinsics.h:6` (`0x0055db78`).
- [ ] Remove game-specific passages from `runtime/README.md` (weanetr data
      imports, DirectInput service threads, game paths).

### 1.2 Small renames and moves

- [ ] Rename `win32_return_chain` to an x86 name (used by `cpu.cpp`, `jit.cpp`).
- [ ] Move the guest memory map (heap, stack, mod heap, TEB, trampolines) out
      of `x86.h` into its own header; `x86.h` keeps only `fs_base`.
- [ ] `host/smoke_main.cpp`: call `recomp_readable_path` rather than
      `win32_host_path_op`.

### 1.3 Split the loader

- [ ] `pe.cpp`: header parsing, section mapping, relocations, import
      enumeration (`slot, width, dll, name|ordinal`), exports, forwarders,
      resources, TLS and exception directory accessors. No guest-OS knowledge.
- [ ] `imports.cpp`: binds enumerated imports to trampolines; only place that
      knows trampolines.
- [ ] Windows process start: TEB, stack, `fs_base`, DLL attach order,
      `LoadLibrary` identity, `run_entry`. Split `loader_init_context` into CPU
      defaults (x87/MXCSR/eflags) and Windows setup (`loader.cpp:1272`,
      `kernel32.cpp:4013`).

`runtime_tests` already cross-checks section mapping against `pefile`.

### 1.4 Split the guest file system out of `kernel32.cpp`

- [ ] `vfs.cpp`, taking component lists: aliases, resolver/lister hook,
      case-insensitive game-directory fallback with listing cache, read-tier
      opens promoted on write, no write fall-through. Case-insensitivity as a
      flag.
- [ ] `win_path.cpp`: Windows path parsing and `win32_guest_path`.
- [ ] `native_seam.h` API unchanged; `mods/overlay.cpp` unchanged.

Covered by `runtime_tests.cpp:4374-4943`.

### 1.5 Split the scheduler out of `kernel32.cpp`

Last and alone: highest risk.

- [ ] `sched.cpp`: baton, runnable selection, deadlines, slices (1 ms) and the
      1024-call checkpoint budget, `sched_atomic_enter`, host thread per guest
      thread, exit unwinding, hand-off log, deadlock report.
- [ ] Waits as records with `try_satisfy` (check and take under the lock, as
      `sched_satisfy_locked` does) and `describe` (for deadlock reports).
- [ ] `kernel32` keeps handle waits, critical sections, mutex ownership,
      suspend counts and APCs as wait records and per-thread data.
- [ ] `GuestThread` holds the CPU context by pointer and Windows data
      (`teb`, `tls`) as personality data.

### 1.6 Split `misc.cpp` by DLL

- [ ] Registry, shell32, ole32, imm32, wsock32, winmm into separate files.

### 1.7 Later, optional

- [ ] Turn `seh.cpp` into one implementation ("Windows x86 SEH") of an
      exception-dispatch interface.
- [ ] Put a guest-input interface between `host/` and `WM_*`/DIK/VK.

## Phase 2: 64-bit Windows

Not started. Notes for when it is.

**What changes**

- *Arguments.* `arg(c, i)` reads 4-byte stack slot `i` (`imports.h:140`), and
  64-bit values take two slots (`kernel32_wide.cpp:792`). Win64 passes the
  first four arguments in RCX/RDX/R8/R9 (XMM0–3 for floats), then on the stack
  after 32 bytes of shadow space; the caller cleans up. About 3,000 sites.
- *Struct layouts.* At least ~940 literal-offset accesses. Pointer-sized
  fields (`HANDLE`, `WPARAM`, `LPARAM`, `SIZE_T`, pointers) become 8 bytes, so
  offsets and sizes move (`MSG`: 28 → 48 bytes).
- *Addresses vs pointer-sized values.* If the 64-bit image and heap are kept
  below 4 GB, arena addresses stay 32-bit throughout the runtime; only the
  pointer-sized values stored in guest memory and arguments widen. Handles are
  sign-extended on Win64 (`INVALID_HANDLE_VALUE`, `-1`/`-2` pseudo-handles).
- *Callbacks.* `guest_call` pushes dwords; Win64 needs registers, shadow
  space and 16-byte stack alignment.
- *COM.* Vtable slots and the 16-byte guest COM header widen.
- *Exceptions.* No FS:[0] chain; table-driven unwinding from `.pdata`/`.xdata`
  (`RtlVirtualUnwind`, `RtlUnwindEx`), which MSVC C++ exceptions rely on.
- *Loader.* PE32+ (`load_main:1035` rejects it; `patch_iat` already handles
  `0x20b`), 8-byte IAT slots, `DIR64` relocations, TLS callbacks, GS-based TEB
  (self `0x30`, PEB `0x60`, TLS `0x1480`) and a minimal PEB.
- *Translator.* x86-64 front end (REX, RIP-relative, 16 registers, SSE2
  floats); p-code needs a register mapping beside `pcode_x86.cpp`; Unicorn
  covers the harness.
- *DirectX.* 64-bit games are mostly D3D11/DXGI/XAudio2; D3D11 is currently
  retired (see `RUSTIFY-plan.md`). The first target game's imports set the
  scope.

**Proposed approach**

- Typed shims: a shim declares its parameter types; a shared wrapper finds
  them for the ABI, writes the result and computes stack cleanup. Removes the
  `argc_stdcall` column. On 32-bit, check the computed count against the
  current column so each conversion is verified. Varargs and the 39 shims that
  touch registers directly keep a raw form.
- Struct layouts declared once by field type, offsets computed per target
  (MSVC default packing, explicit `pack` where needed), with `static_assert`s
  against today's 32-bit offsets.
- Convert file by file, `kernel32` first, `dx/` last; a script counts
  remaining raw `arg(` uses.

**Open decisions**

- Keep the 64-bit guest below 4 GB? Assumed yes. Images without relocations
  then fail with a diagnostic. A sparse 64-bit arena works on desktop but not
  under iOS/Android address-space limits.
- Hand-written layouts and signatures, or generated from win32metadata?
  Leaning hand-written.
- "32 and 64 in the same go": one kit that runs either kind of executable,
  one per process, is the normal path (typed shims compiled for both). Mixing
  both in one process (Wine-style WoW64) is out of scope.
- Choice of first 64-bit target game.

## Beyond

macOS (Mach-O) guests need entirely different shims: libSystem, dyld binding,
the Objective-C runtime, AppKit and Metal/OpenGL. The reusable part is the
generic layer plus the scheduler and file system extracted in phase 1. macOS
programs rely on preemptive threads and libdispatch, which may strain the
one-runnable-thread scheduler. Other architectures mainly need the shim
signature `void (*)(X86*)` replaced by the typed shims from phase 2.
