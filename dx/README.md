# `dx` — the graphics, audio, video and input shims

The Windows API surface a recompiled game expects from the DirectX and media
DLLs: DirectDraw through Direct3D 11, DirectSound and WinMM, DirectInput and
XInput, and the audio, video and networking libraries games of that era bundled
(Miles, QMixer, FMOD, Bink, DirectShow, Media Foundation, MLDPlay). Each file
implements a published API contract and nothing about a game: a shim answers
the way the real DLL would, and where it cannot, it says so once in the log
rather than inventing an answer.

`BinkCopyToBuffer` supports unscaled copies and horizontal pixel doubling
(`flags & 0x70000000 == 0x30000000`), preserving the supplied destination pitch.
The original `binkw32.dll` dispatch at `0x1000d778` selects pixel pairs; its
XRGB writer at `0x1000bd9f` stores each pixel twice. Other scaling modes remain
unsupported and report an error. Destination validation includes the expanded row.

Every module is compiled and `dx_register_shims()` registers all of them; a
build does not select by game or era. A game reaches only the ones its imports
name, so the rest cost registration, not behaviour. The work behind them is
split three ways: the DX3-7 recorder, audio and input leave through callbacks
in `host_api.h`; D3D8 and D3D9 hand their draws to the Rust
`graphics/d3d8-wgpu` crate; D3D11 renders through `host/gpu2d` on the host GPU.

## By era and subsystem

| Era / subsystem | Files | What it covers |
| --- | --- | --- |
| Common | `com.h/.cpp`, `dx.h/.cpp`, `host_api.h/.cpp`, `display_stubs.cpp`, `coherence.h`, `passes.h`, `frame_dump.h/.cpp` | guest COM objects and vtables; `dx_register_shims()` and `dx_reset()`; the host callback seam with weak no-op defaults; display coherence and frame/draw mapping hooks; frame capture |
| DirectDraw and Direct3D 2-7 (DX3-7) | `ddraw.cpp/.h`, `d3d.cpp`, `d3d7.cpp`, `dxtypes.h`, `dxt_decode.h` | IDirectDraw/2/4 with surfaces/2/3/4, palettes and clippers; IDirect3D/2 and IDirect3D7 devices, viewports, materials, lights and textures; DX6 constants and DXT decode |
| Direct3D 8 (DX8) | `d3d8.cpp`, `d3d8_fpu.h`, `d3d8_bindings.json`, `d3d8_constants.json` | IDirect3D8 factory, device, surface, texture, vertex-buffer and index-buffer vtables behind the Rust renderer; the guest x87 control-word change `CreateDevice` implies |
| Direct3D 9 (DX9) | `d3d9.cpp`, `d3dx9.cpp` | IDirect3D9 device and resources over the Rust renderer; D3DX9 matrix/vector maths and effect loading |
| Direct3D 10/11 and DXGI | `d3d11.cpp/.h`, `dxgi.cpp`, `d3dx10.cpp`, `d3dcompiler.cpp` | ID3D11Device/context/resources for one fixed 2D quad pipeline; IDXGISwapChain over guest-owned back buffers; D3DX10 row-vector maths; a tagged-source `ID3DBlob` shim |
| DirectSound and WinMM audio | `dsound.cpp`, `waveout.cpp`, `audio3d.h` | IDirectSound, buffers, 3D buffer/listener and notification positions; WinMM `waveOut` queues; the 3D model shared with Miles |
| DirectInput and XInput | `dinput.cpp`, `dinput_joystick.cpp/.h`, `xinput.cpp` | IDirectInputA mouse and keyboard; the virtual pad as a DirectInput joystick; the virtual pad as XInput user 0 across `xinput1_3`, `1_4` and `9_1_0` |
| Bundled audio libraries | `mss32.cpp`, `redbook.cpp`, `qmixer.cpp`, `fmod.cpp`, `soundlib.cpp`, `galaxy_stub.cpp`, `mp3_source.cpp/.h`, `riff.cpp/.h` | Miles drivers, PCM/MP3 streams and CD audio; the 28 QMixer exports; FMOD 3 samples and streams; the Soundlib MIDI helper; a null Galaxy API; shared MP3 and RIFF readers |
| Video and cinematics | `avi.cpp`, `dshow.cpp`, `bink.cpp`, `video_frame.cpp/.h`, `mf.cpp`, `mf_media.cpp/.h` | read-only AVIFile streams; the DirectShow reading side; Bink containers and video; shared decoded-frame helpers; Media Foundation graph playback over the FFmpeg reader |
| Networking | `weanetr.cpp` | the 17 MLDPlay methods, reporting no networking (`GetCurrentMs` excepted) |
| Tests | `tests/` | `dx_tests` for the shim paths, `pad_tests` for the DirectInput joystick and XInput, plus the D3D8 descriptor/rendering and D3D7 mipmap suites |

DirectInput gamepad feedback supports constant and periodic effects. Directional
wheel forces are approximated on both rumble motors; condition and ramp effects
are unsupported. Short periodic effects use their mean to avoid aliasing.

## Renderers

| Layer | Rasterization | Notes |
| --- | --- | --- |
| DX3-7 (`ddraw`, `d3d`, `d3d7`) | the kit's own, `host/d3d_render.cpp` over `host/gpu/` | fixed-function immediate mode; vertex lighting is recorded but not applied |
| D3D8 (`d3d8.cpp`) | the Rust `graphics/d3d8-wgpu` crate (wgpu) | optional; without it the no-renderer path advertises no adapter |
| D3D9 (`d3d9.cpp`) | the Rust `graphics/d3d8-wgpu` crate (wgpu) | base D3D9 (no D3D9Ex) and `d3dx9_33` effects; vs 1.1/2.0/2.1 and ps 1.1-2.1 instruction subsets without flow control or predication, so SM3, loops/branches, `texldl`/`sincos` and ps 1.x texture-register ops fail by name; also refused: multisampling, MIRRORONCE, point sprites, P8 palettes, effect struct parameters and pool-shared samplers. `EvictManagedResources` keeps residency; INTZ samples through the comparison sampler; occlusion counts on Metal are the covered viewport area |
| D3D11 (`d3d11.cpp`) | the kit's own, `host/gpu2d.cpp` | one fixed 2D layered-quad pipeline; `d3dcompiler.cpp` is a tagged-source blob, not a compiler |
| Video | FFmpeg (`mf_media.cpp`, `avi.cpp`, parts of `bink.cpp`, `waveout.cpp` ADPCM), minimp3 (`dshow.cpp`, `mp3_source.cpp`), Bink video in `bink.cpp` | containers, demux and frame helpers are kit code |

## Audio

Every playback shim — DirectSound, QMixer, Miles, WinMM `waveOut`, FMOD,
redbook and the audio track of Bink, DirectShow and Media Foundation — shares
one host mixer (`host/audio/mixer.cpp`) and one channel allocator
(`dx_alloc_audio_channel` in `dx.cpp`), so the host sees a single flat voice
space. MIDI (`soundlib.cpp`) is the exception: it uses a separate `host_midi_*`
seam over `host/audio/midi_synth.cpp`. Wave-in capture has no implementation:
the WinMM `waveIn*` imports are stack-balancing stubs in `runtime/misc.cpp`.

## Bounded D3D8 bridge

`d3d8.cpp` registers complete factory, device, surface, texture, vertex-buffer
and index-buffer vtables. Implemented operations cover adapter facts and
conservative capabilities; a single explicit-size windowed color target with
backbuffer and implicit-surface lifetime; render-target and depth-stencil
surfaces; 2D textures with mip levels in the uncompressed and DXT formats the
CPU storage holds; vertex and index buffers with lock/unlock; the scene, clear,
transform, viewport, material, light, render-state, texture-stage and texture
operations; vertex and pixel shader creation, constants and declarations; and
the host ABI's owned-pixel presentation. The rest fail by name through the
import dispatcher rather than returning a plausible value: among them state
blocks, palettes, volume and cube textures, extra swap chains, cursor and
clip-plane calls, the transform/render-state/stream/indices getters,
`DrawIndexedPrimitiveUP`, patch drawing and the private-data slots. Backend
failures never become success.
Capability bits are not inflated to pass selection. DIVERGENCE(original):
`SetGammaRamp` is a no-op with a one-time diagnostic; host gamma is unchanged
and gamma capability bits stay clear. `CreateDevice` also models the guest x87
control-word change unless `D3DCREATE_FPU_PRESERVE` is set (`d3d8_fpu.h`).

Native desktop builds require Rust and statically link `graphics/d3d8-wgpu`
through CXX. A game's own crate at that path takes precedence. Rust owns D3D7/D3D8
CPU state and resources even with GPU rendering disabled; the null host uses the
same CPU implementation. Enable GPU rendering with `[render] d3d8_wgpu = true`
in the game configuration when building through `tools/build.py`. Cross builds require explicit Rust target
integration. COM IIDs and tables are generated from a pinned
Wine header plus `d3d8_bindings.json`; see [Testing](../docs/testing.md).

## Wiring it up

`imports_init()` runs from `loader_load()` and installs logging-only entries
for these DLLs. `dx_register_shims()` replaces them, so the host must call it
**after** `loader_load()`:

```c
loader_load(...);        /* runs imports_init(), patches the IAT */
dx_register_shims();     /* upgrades the DirectX entries in place */
```

`imports_register` upgrades an already-allocated trampoline in place, so the
addresses the loader wrote into the IAT stay valid and no runtime file needs
to change. Registering twice is a no-op.

After `mem_init()` — which discards the guest heap the COM objects and
vtables live in — call `dx_reset()` before using any of this again.

## How a COM object works

A guest interface pointer is a 16-byte header on the runtime heap:

| offset | field |
| --- | --- |
| 0 | `lpVtbl`, the guest address of the interface's vtable |
| 4 | `COM_MAGIC`, so a stray pointer is caught rather than followed |
| 8 | index of the host-side `ComObj` |
| 12 | which `ComIface` this view is |

The vtable is a second guest allocation holding one import trampoline per
slot, from `imports_alloc_trampoline`. A guest `call [vtbl + 4*n]` therefore
lands in `imports_dispatch`, which runs the slot's shim and then pops the
return address plus `4*argc` exactly as the real `ret n` would. **A shim never
touches ESP**, matching the contract in `runtime/imports.h`.

One host object can be reached through several interface views. An
`IDirectDraw2` and the `IDirectDraw4` a `QueryInterface` returns are two guest
pointers with two vtables and one shared refcount, as COM requires. Views are
allocated on first use and cached, so asking twice returns the same pointer.

Three relationships cross object boundaries and are handled by a per-kind
`QueryInterface` hook: a DirectDraw object asked for `IID_IDirect3D2` returns
the Direct3D object it owns; a surface asked for `IID_IDirect3DTexture2`
returns itself; a sound buffer asked for the 3D or notify interfaces returns
itself.

**Slot order is the entire contract.** Each vtable is written out in full,
including the slots that are stubs, because an interface that stopped early
would send the guest to the wrong method rather than to a missing one. The
tests assert the slot count and that no two slots share a trampoline.

## Interface inventory

A game's real DirectX surface is the set of IIDs its code references, not the
set present in `.rdata`: `dxguid.lib` links all 42 of them, so presence proves
nothing. Cross-referencing the executable's `.text` against the IID table is
what produces the inventory. The version-1 interfaces
(`IID_IDirectDraw`, `IID_IDirectDrawSurface`, `IID_IDirectDrawPalette` and
`IID_IDirectDrawClipper`) are often never requested by IID, but they are still
implemented in full because `DirectDrawCreate` returns one and `CreateSurface`
hands one back.

Vtable slot offsets were checked against real call sites in the Ghidra
decompilation rather than taken on trust: `IDirect3D2::FindDevice` at `+0x1c`
and `CreateDevice` at `+0x20`, `IDirectDraw2::CreatePalette` at `+0x14` and
`IDirect3DDevice2::EnumTextureFormats` at `+0x24`, `SetRenderState` at
`+0x5c`, `IDirect3DViewport2::SetViewport2` at `+0x44`, and
`IDirectDraw::SetCooperativeLevel` at `+0x50`.

## Behaviour worth knowing before you debug something

- **Surface pixels are guest memory.** A `Lock` hands back a real guest
  address, the guest writes through it, and the memory comparison fixture can compare
  the result. Pitch is the row width rounded up to 16 bytes, as a real driver
  aligns it.
- **`Flip` swaps the pixel pointers** of the front and back surface rather
  than copying. That is what retargeting the scan-out does, and it is what the
  guest's next `Lock` of either surface has to see.
- **Presentation happens on `Flip`, on `Unlock` of the primary, on a `Blt` or
  `BltFast` whose destination is the primary, on `ReleaseDC`, and on
  `SetPalette` or `SetEntries` affecting the visible surface.** The last one
  matters: attaching or editing a palette changes what is on screen without
  moving a pixel.
- **A back buffer never presents.** Only the visible surface does; a back
  buffer becomes visible by being flipped, not by being drawn into.
- **`GetDeviceIdentifier` reports vendor and device 0 on purpose.** The game
  matches these against a list of 1998 chipsets (Riva 128, Riva TNT, Rage Pro,
  Voodoo2, G200, Permedia2, Banshee) and takes a per-card workaround path on a
  hit. Zero is "unrecognised", which is the generic path and the only one this
  shim can honestly claim to implement.
- **The Direct3D device advertises the full documented blend, compare and
  texture-blend cap sets.** This is not padding: `init_d3d` rejects a device
  outright unless specific `dpcTriCaps` blend and alpha-compare bits are set.
  Advertising a superset is a promise the native renderer has to keep.
- **Direct3D records, it does not rasterize.** Every draw becomes one
  `host_d3d_draw` carrying the vertices, the render-state snapshot, the active
  transforms, the viewport and the texture handle. The immediate-mode
  `Begin`/`Vertex`/`Index`/`End` path accumulates into guest memory and emits
  the same single command, so the host sees one shape of input.
- **`GetDC` hands out a unique HDC but GDI cannot draw into surface memory**
  here. Anything rendered through that DC is lost. It logs once.
- **DirectInput deltas are consumed on read.** `host_input_state` clears them,
  so `poll_device` is the only reader; a second read in the same call would
  lose that frame's motion.
- **Nothing drives the mixer from a guest thread, so the shims are driven from
  the message loop.** `QSWaveMixPump` is for an application that wants to pump
  QMixer by hand and may never be called; a streamed wave hung only off it
  would play its opening chunks and stop. `dx_register_shims` registers
  `qmixer_frame_pump` with `host_set_frame_pump`, and the guest's own message
  loop runs it between frames. DirectSound's notification service rides along
  on the same tick.
- **QMixer's `PauseChannel` stops rather than pauses**, because there is no
  host pause; `RestartChannel` replays from the start. It logs once.
- **A looping buffer the guest writes into is a stream, not a loop.** This is
  how a game plays music and speech. A typical writer creates a 32768-byte
  buffer at 22050 Hz stereo 16-bit with `GETCURRENTPOSITION2 | CTRLVOLUME |
  CTRLPAN | CTRLFREQUENCY`, reads its length back out of `GetCaps`, and calls
  `Play(0, 0, DSBPLAY_LOOPING)` while it is still empty. A thread then polls
  `GetCurrentPosition` every ten milliseconds and, every forty, locks three or
  four kilobytes at its own running offset - 0, 3584, 7056, 10640, wrapping at
  32768 - staying about a lap ahead of the play cursor. There are no
  notification positions on that path.

  So the first lap really is silence, in the original as much as here. What
  must not happen is re-submitting the ring on each `Unlock`, which restarts
  the voice twenty-five times a second. Ring order from the play cursor is
  play order, so the first write converts the buffer: the rest of the current
  lap is re-issued as a one-shot and everything written afterwards is appended
  with `host_audio_queue`. The cursor the guest polls is then derived from what
  the host has left to play, which is what paces the guest's own writer. A
  looping buffer nobody writes into stays a loop.

  `host_audio_stream` does the conversion at the cursor: it stops the voice
  once and reschedules immediately from where it had reached, so nothing is
  repeated and nothing is skipped, but there is a scheduling seam of a
  millisecond or two. That is deliberate. Handing over at the loop point
  instead would avoid the seam and cost up to a whole extra lap of the ring
  first - 370 ms at 32 KB and 22050 Hz stereo - on a buffer holding silence at
  that instant. It happens once per sound; everything after it is appended and
  gapless. It reports the offset it resumed from, and
  `host_audio_played_bytes` counts on from there at the sample rate and never
  goes backwards, across a re-schedule, across a refill and across an
  underrun. Reducing that modulo the ring is the whole of the play cursor.

  It has to be a count of what has been played and not of what is in flight.
  A guest keeps about a lap of audio in flight on purpose, so a cursor derived
  from the outstanding bytes sits a fixed distance ahead of its own write
  offset - and that distance is exactly what the refill gate measures (the
  bytes from the write offset forward to the cursor, compared against the next
  chunk's length). Freeze it and the guest stops writing, which stops the
  stream, which freezes it further: the movie waits on a number that will not
  move, with nothing to show that anything is wrong.

  A host that answers -1 to `host_audio_stream` cannot continue a sound, and
  the ring stays a loop that is re-submitted at every refill. It says so once.
- **DirectSound notification positions are signalled.** A second streaming path
  builds a worker thread and two events, registers them at offset 0 and the
  half-way point of a `0x7800` buffer and calls `Play`, then parks the worker
  in `WaitForMultipleObjects` on them. Recording positions without signalling
  them would hang that worker for good. `dsound_pump()` compares the host play
  cursor against the registered offsets and signals what it has passed;
  `QSWaveMixPump` calls it because the guest calls that every frame, and every
  DirectSound entry point calls it too.
- **A streamed wave is refilled through `host_audio_queue` where the host has
  it, and re-submitted where it does not.** Re-submitting restarts the voice,
  so the join is audible; it logs once when it falls back. The chunk the guest
  callback returns zero on is still played, because zero means "this was the
  last one", not "this one is bad".
- **weanetr reports failure so networking is unavailable.** The polarity is
  not a guess: a caller frees its MLDPlay object when `StartupNetwork` returns
  zero, so zero is failure across MLDPlay. `GetCurrentMs` is the exception and
  returns the real clock, because callers use it from dozens of places that
  have nothing to do with networking.

## Known gaps

- **`QSWAVEMIXOPENWAVEDATA`'s layout is unconfirmed.** The game builds it in a
  stack slot Ghidra could not reconstruct and no dynamic trace existed when
  this was written. `locate_wave` therefore accepts the pointer itself being
  RIFF data, or any of the first 16 fields pointing at RIFF data, and logs
  once when it finds neither. A wave it cannot decode still gets a valid
  handle, so the game proceeds silently rather than failing.
- **Blits between surfaces of different bit depths copy raw pixels** instead
  of converting. It logs once.
- **`DDBLT_KEYDEST` and `DDBLT_ROP` are ignored**, each with one log line.
- **Positional audio is stored, not applied**, in both DirectSound and QMixer,
  for the native host to consume when needed.

## Diagnosing a missing sound

`dx_dump` ends with a QMixer tally: waves opened and refused by reason, plays
asked for against plays delivered, and every drop counted under its cause - no
channel, channel disabled, session inactive, no host voice left, wave empty,
stream dry. The first twenty drops print a line each. "Some sounds are
missing" cannot be answered by a log of successes, and it was two counted
drops that found both of the bugs below.

Both were guesses about an SDK whose header this project does not have, and
both are now settled from evidence rather than from the guess:

- **`QSWaveMixOpenChannel`'s third argument selects what the second means.**
  QMixer.dll switches on it with `cmp eax,3` and a four-entry jump table, so it
  is 0 to 3. The game passes 2 with 15 and then plays on channels 0 upwards, so
  2 is a count. Reading it as an index left every channel the game uses
  unopened.
- **`QSWaveMixEnableChannel`'s fourth argument is not read as a mute.** The
  game calls it with 0 and then plays on the same channel and expects to hear
  it. Reading that as "disable" dropped every sound on the channel. The request
  is recorded and said once; playback is not gated on it.

- **`QSWaveMixSetVolume`'s value is a linear amplitude on 0 to 32767.** Its
  parameter handler in QMixer.dll is three instructions - `fild [edi+4]`,
  `fmul [0x180244d4]` where that constant is 3.051851e-05 or 1/32767, `fstp`
  into the channel's gain - so gain is value/32767. The game passes 14190,
  which is 0.433 or -7.3 dB. Read as hundredths of a decibel, as it was, every
  positive number clamped to unity and every sound played at full volume with
  no mix at all. A channel nobody has set is at full scale, because zero on a
  linear scale is silence rather than unity.

The 28 export arities are confirmed against the real QMixer.dll, so nothing on
that path is corrupting the guest stack.

## Diagnosing a silent run

`RECOMP_AUDIO_TRACE=N` prints the first N audio events: every `Lock`, `Unlock`,
`GetCurrentPosition` and submission on a playing buffer, with the offsets, the
lengths and the peak of what was written. It is off by default and costs
nothing when off.

The point of it is that a stream which goes quiet is one of a small number of
things, and they are told apart by which line stops appearing. No `Lock` at
all means the guest is not asking, and for the video player that means its own
cursor gate never opened, which means `GetCurrentPosition` is not advancing.
`Unlock` with `peak 0.000` means the guest is asking and writing silence.
`Unlock` with a real peak and no submission after it means the write is not
reaching the host. `build/recomp/recomp_headless` boots into the same intro, so
all of this can be read off a headless run:

```
RECOMP_AUDIO_TRACE=200 RECOMP_MAX_SECONDS=25 build/recomp/recomp_headless
```

Other audio diagnostics, all off by default:

- `RECOMP_AUDIO_DUMP_BUFFERS=<dir>` writes the PCM of each distinct sound the
  guest plays as `buf<id>_<hash>.wav`, in the buffer's own format. It shows
  what the guest decoded, which tells a bad decode from a bad schedule.
- `RECOMP_TRACE_IMPORTS=<substring>` logs every import whose `dll!name`
  contains the substring, with its stdcall arguments (hex and as float) and the
  guest return address, for example `IDirectSound3D`.
- `RECOMP_TRACE_FILES=1` logs file opens and reads, including `mmioOpenA`.

## DirectSound3D

3D buffers (`DSBCAPS_CTRL3D`, or a buffer asked for `IDirectSound3DBuffer`) are
attenuated and panned from the listener and buffer positions, min/max distance
and rolloff, following Wine's `DSOUND_Calc3DBuffer` (see the comment above
`calc_3d` in `dsound.cpp` for the formulas and what is from a reference versus
memory). Cones, velocity/Doppler and modes other than normal are logged once
and not applied. The Windows result for the same inputs has not been compared.

## Tests

```
.venv/bin/python tools/test.py --native          # build and run
.venv/bin/python tools/test.py --compile-only    # build only
.venv/bin/ctest --preset macos -R dx_tests       # one suite
```

Run from the repository root. 4540 checks: vtable integrity, the
gradient/flip/present path the task specifies, blitting and colour keys,
`QueryInterface` identity, display-mode enumeration, the full Direct3D
pipeline through to a textured indexed draw, the DirectSound data path from a
`Lock` pointer to the bytes the host is handed, DirectSound notification
positions driven across the loop point, DirectInput state and buffered events,
a QMixer session, streamed QMixer waves fed by a synthetic guest callback on
hosts with and without a queue, weanetr's failure codes, and reference
counting against the heap block count. The audio groups are driven from the
Wine trace of the original: the streaming test uses its buffer size, format,
lock offsets and poll positions verbatim.

Every call goes through the real guest path: arguments are pushed onto a guest
stack and the shim is reached through `imports_dispatch`, the same way
recompiled code reaches it. The harness asserts after every call that the
shim popped exactly its own arguments and returned to the pushed address,
which is how a wrong `argc` in a vtable is caught. The suite is clean under
`-fsanitize=address,undefined`.

D3D8 `DrawPrimitiveUP` reads checked guest arena spans, uploads immutable vertex
snapshots and clears stream zero after the call. Nonindexed triangle strips
and fans expand into triangle lists with preserved winding. The renderer also
accepts the 28-byte `XYZRHW | DIFFUSE | TEX1` FVF (0x144); its vertex entry
supplies zero specular without reading a nonexistent field. Lines and unsupported
vertex/state combinations remain named failures. The API data lifetime and
stream-zero contract follows Microsoft's D3D8 documentation (ms889296).

DirectSound enumeration reports the NULL-GUID primary-driver alias and a concrete
Recomp Software Playback device with a stable bridge-owned GUID. Both create
the same software mixer; unknown GUIDs are rejected. Enumeration honors a
callback returning FALSE and keeps callback strings/GUIDs in guest memory.

DirectInput7 creation uses the shared version-1/2/7 prefix with FindDevice
and CreateDeviceEx at their documented slots. CreateDeviceEx validates the
requested IID and returns a version-7 device view with the existing mouse/
keyboard behavior. Name search and effect-file operations return named
E_NOTIMPL diagnostics. Interface layouts follow Wine include/dinput.h.
