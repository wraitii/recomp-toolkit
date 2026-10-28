// dx.h - what the host application and the runtime call into for the
// graphics, audio and input shims.
//
// The runtime registers logging-only entries for DDRAW/DINPUT/DSOUND/QMIXER/
// weanetr in imports_init(). dx_register_shims() replaces them with these
// implementations, so the host must call it AFTER loader_load():
//
//     loader_load(...);        // runs imports_init(), patches the IAT
//     dx_register_shims();     // upgrades the DirectX entries in place
//
// imports_register upgrades an already-allocated trampoline in place, so the
// IAT addresses the loader patched stay valid and no runtime file needs to
// change. Registering twice is harmless.
#pragma once
#include "../runtime/guest.h"
#include <stdio.h>

// Registers every DirectDraw, Direct3D, DirectSound, DirectInput, QMixer and
// weanetr shim and builds the COM vtables in guest memory. Idempotent.
void dx_register_shims();
void dx_report_failure(const char *desc, uint32_t hr, uint32_t caller);

// Drops every COM object and rebuilds the vtables. Call after mem_init(),
// which discards the guest heap the objects and vtables live in.
void dx_reset();

// Per-module registration, exposed for the tests. dx_register_shims calls all
// of these; each is idempotent.
void ddraw_register();
void d3d_register();
// Direct3D 7: the IDirect3D7 factory, IDirect3DDevice7 state store and
// IDirect3DVertexBuffer7. Separate from d3d.cpp because the D3D7 tables have a
// different slot order from Direct3D 3.
void d3d7_register();
void d3d7_reset();
void d3d9_register();
void d3dx9_register();
// Direct3D 8: the factory/device/resource bridge onto the Rust wgpu renderer.
void d3d8_register();
void d3d8_reset();
void dsound_register();
void dshow_register();
void dinput_register();
void xinput_register();
void qmixer_register();
void mss32_register();
void redbook_register();
void avi_register();
void waveout_register();
void waveout_frame_pump(X86 *c);
void waveout_shutdown();
void redbook_frame_pump(X86 *c);
void fmod_register();
void soundlib_register();
void galaxy_stub_register();
void bink_register();
void weanetr_register();
// Media Foundation: the session, the topology it is given and the file it
// plays. Registered last of the media shims because nothing else defers to it.
void mf_register();
// Drops the decoders, audio channels and event queues before com_reset
// discards the objects that named them.
void mf_reset();

// Per-module state resets. Each drops the cached guest addresses and handle
// tables that pointed into the arena mem_init discarded. dx_reset calls them
// all; the guest heap those records point into is gone by then.
void ddraw_reset();
void d3d_reset();
void d3d9_reset();
void d3dx9_reset();
void dsound_reset();
void dshow_reset();
void dinput_reset();
void bink_reset();
void fmod_reset();
void soundlib_reset();

// Close open video players while the guest heap and host audio are still alive.
// Call after guest workers stop, before tearing down the host.
void bink_shutdown();

// The host calls this after feeding new input through host_input_state. It
// signals the notification event of every DirectInput device that registered
// one, which is what wakes a guest thread waiting on it so it can call
// GetDeviceData. Safe to call from a host thread while guest code is running.
// C linkage on purpose: this is the one symbol in this header a host calls
// from outside C++, and a host that declares it extern "C" against a mangled
// definition links to nothing. With a weak declaration on the host side that
// failure is silent - the address is null, the guarded call is skipped, and
// input stays dead with nothing to show for it.
extern "C" void dinput_host_input_changed(void);
void qmixer_reset();

// Requested against delivered for the QMixer path: how many sounds the game
// asked for, how many reached the host, and what happened to the rest.
// "Some sounds are missing" cannot be answered by a log of successes.
void qmixer_dump(FILE *out);

// Services DirectSound notification positions: signals the guest events whose
// offsets the play cursor has reached since the last look. The guest thread
// that streams into a looping buffer is parked in WaitForMultipleObjects on
// exactly those events, so nothing refills that buffer until this runs.
// QSWaveMixPump calls it because the game calls that every frame; every
// DirectSound entry point calls it too.
void dsound_pump();

// Runs on the main guest thread between frames, from the guest's own message
// loop, registered with host_set_frame_pump by dx_register_shims. It refills
// streamed QMixer waves by calling the guest back for more samples and it
// services DirectSound notification positions.
//
// It exists because the game drives neither. Real QMixer runs its own mixing
// thread, so QSWaveMixPump is called zero times in a whole run, and a streamed
// wave without a tick plays the chunks it was given at the start and then
// stops. DirectSound's streaming worker is parked in WaitForMultipleObjects on
// events nothing else would signal.
void qmixer_frame_pump(X86 *c);
// Services Miles sample completion and timer callbacks on the guest frame seam.
void mss32_frame_pump(X86 *c);
// Drops the Miles sample/driver/3D state, whose guest addresses and host
// channels the arena teardown is about to discard.
void mss32_reset();
void mss32_shutdown();
// Decode FMOD streams and schedule Soundlib MIDI events on the guest thread.
void fmod_frame_pump(X86 *c);
void soundlib_frame_pump(X86 *c);
// Keeps a DirectShow graph's audio channel fed and posts its completion.
void dshow_frame_pump(X86 *c);
// Advances a playing media session: decodes to the wall clock, presents the
// video frame that is due, tops the audio stream up, and hands the player the
// events it armed for with BeginGetEvent.
void mf_frame_pump(X86 *c);
// True while a media session is presenting. The display shim asks before it
// presents the guest's primary: both reach the one screen, and the game draws
// far more often than a movie has frames, so without this the picture the
// player sees is the game's, with the movie flickering underneath it.
extern "C" bool mf_owns_the_screen();
// The display's frame boundary in the same shape, registered beside it: the
// display's frame ends where the guest's message loop pumps, and that fact
// lives with the display shim rather than inside the audio pump.
void ddraw_frame_pump(X86 *c);
// Test only: forget the cached RECOMP_QMIX_GATE reading.
void qmixer_gate_reset_for_test();
// Test only: XInputEnable's stored state and the keystroke replay position.
void xinput_reset_for_test();

// A frame presented by a device that renders a whole CPU image itself: begin
// before host_present, end right after it, which seals the frame.
void ddraw_external_present_begin(void);
void ddraw_external_present_end(void);

// Diagnostics: live COM objects, surfaces and audio channels.
void dx_dump(FILE *out);

// ---------------------------------------------------------------------------
// Cross-module hooks. Declared here so com.cpp's QueryInterface can hand a
// DirectDraw object the Direct3D object it owns without ddraw.cpp and d3d.cpp
// including each other.
// ---------------------------------------------------------------------------
struct ComObj;
ComObj *dsound_new_device();
ComObj *dsound_new_primary_buffer(uint32_t rate, uint32_t bits, uint32_t channels, uint32_t flags);
// The IDirect3D(2) object belonging to a DirectDraw object, created on demand.
ComObj *d3d_for_ddraw(ComObj *dd);
// Presents the primary surface through host_present. Called on Flip, on
// Unlock of the primary and on a Blt whose destination is the primary.
void ddraw_present(ComObj *primary);
// The palette that governs an 8-bit surface: its own, else the primary's.
const ComObj *ddraw_effective_palette(const ComObj *surface);
// The palette that governs an 8-bit surface: its own, else the primary's.
const ComObj *ddraw_effective_palette(const ComObj *surface);
// Uploads a texture surface to the host renderer, for IDirect3DTexture2::Load
// and for the first GetHandle.
void d3d_upload_texture(ComObj *surface);
// Current surface content revision for a texture handle; zero if invalid.
uint32_t d3d_texture_revision(uint32_t handle);
// The Direct3D device rasterizes into a DirectDraw surface, so anything it has
// drawn has to be in that surface's own memory before the guest reads it,
// writes it or presents it. ddraw.cpp calls this from Lock, Blt, BltFast,
// GetDC, Flip and the present path; it is a comparison and a return for every
// surface that is not the current render target.
// `why` names the call that asked, so one frame's worth of flushes and blits
// reads as a sequence rather than as a pile of identical lines.
void d3d_flush_surface(ComObj *surface, const char *why);
// The D3D7 equivalent: if `surface` is the Direct3D 7 device's render target,
// read the Rust device's 32-bit target back and convert it into the guest's
// 16bpp bytes. Called beside every d3d_flush_surface above. A no-op for every
// surface no D3D7 device renders into.
void d3d7_flush_surface(ComObj *surface);
bool d3d7_stage_surface(ComObj *surface);

// ---------------------------------------------------------------------------
// D3D7 -> D3D8 state translation (dx/d3d7.cpp). Pure functions so dx_tests can
// check the table without a GPU. Values are the raw D3D7 and D3D8 enum members.
// ---------------------------------------------------------------------------
typedef enum {
    // Forward the value unchanged to *d3d8_state (the two enums agree).
    D3D7_STATE_FORWARD = 0,
    // No D3D8 state exists; ignoring this value is exact for the value the
    // game uses (documented at the call site).
    D3D7_STATE_IGNORE = 1,
    // No D3D8 state exists and ignoring would change behavior: fail loudly.
    D3D7_STATE_INVALID = 2,
} D3d7StateMap;
D3d7StateMap d3d7_translate_render_state(uint32_t d3d7_state, uint32_t value, uint32_t *d3d8_state);
// Maps a D3D7 transform state to its D3D8 member (WORLD 1 -> 256, VIEW 2,
// PROJECTION 3, texture 16..23). Returns false when there is no equivalent.
bool d3d7_translate_transform(uint32_t d3d7_state, uint32_t *d3d8_state);
// Maps a D3D7 texture-stage state to one or two D3D8 states. `out` holds at
// most two; the count is returned, or -1 when there is no equivalent.
int d3d7_translate_texture_stage_state(uint32_t d3d7_type, uint32_t out[2]);
// 8:8:8 -> 5:6:5 and back. The forward direction truncates the low bits (a
// 32-bit internal target copied to the 16bpp guest back buffer); the reverse is
// what the headless presenter does when it expands a 16bpp frame for a PNG.
// Bit replication and a /31 scale differ only in the middle values; the
// presenter uses the scale, so the round trip is documented against it.
uint16_t d3d7_rgb888_to_rgb565(uint32_t rgba);
uint32_t d3d7_rgb565_to_rgb888(uint16_t rgb565);
// Reconcile a tightly packed `w`x`h` RGBA8 block (the Rust target's readback)
// into a guest surface's storage: 16bpp writes R5G6B5, 32bpp writes X8R8G8B8
// with the alpha byte zeroed. `dst` is the surface's HOST pointer (gm_ptr of
// its guest address) and `pitch` its byte pitch. Writes directly, so the store
// does not run the guest write hook (this is the shim's own copy, not a guest
// store). Returns false for an unsupported bpp and writes nothing.
bool d3d7_store_rgba_surface(uint8_t *dst, uint32_t pitch, uint32_t bpp, uint32_t w, uint32_t h,
                             const uint8_t *rgba);
// D3D7 trace helpers (dx/d3d7.cpp), exposed so dx_tests can check the pure
// parts without a GPU. `d3d7_trace_vertex` decodes one vertex per the FVF the
// same way the trace's vertex dump does. `d3d7_trace_parse_frames` parses the
// RECOMP_TRACE_D3D7_FRAMES grammar (1-based, inclusive: "a-b", "a-", "-b",
// "a"); it returns false and leaves the outputs untouched for an empty or
// malformed string.
std::string d3d7_trace_vertex(uint32_t fvf, const uint8_t *v);
bool d3d7_trace_parse_frames(const char *s, uint32_t *lo, uint32_t *hi);
// Collapse helper for the small-draw trace (RECOMP_TRACE_D3D7_SMALL). The
// caller feeds one digest per frame describing the decoded small draws and
// their state. `step` returns true when the digest changed (the caller then
// prints that frame's draws); when it changed and the previous run spanned
// more than one frame, `collapsed` is set to a "frames a-b: unchanged" line
// to print first. `flush` closes a run left open at trace reset. `first` and
// `last` are the inclusive 1-based frame range of the run being collapsed.
struct D3d7TraceSmallCollapser {
    std::string digest;
    bool active = false;
    uint32_t first = 0;
    uint32_t last = 0;
};
bool d3d7_trace_small_step(D3d7TraceSmallCollapser *c, uint32_t frame, const std::string &digest,
                           std::string *collapsed);
bool d3d7_trace_small_flush(D3d7TraceSmallCollapser *c, std::string *collapsed);
// Re-states the render target's memory to the host. Flip swaps the pixels
// behind a surface, so the host has to be told when its target moves.
void d3d_retarget_surface(ComObj *surface);
// The surface is being destroyed. If the device was rendering into it, the
// device has no target any more: the host must not be left holding a pointer
// into memory that is about to be freed.
void d3d_forget_surface(ComObj *surface);
// Allocates the next free audio channel; DirectSound buffers and QMixer
// channels share one numbering so the host mixer sees a flat channel space.
int32_t dx_alloc_audio_channel();
void dx_free_audio_channel(int32_t ch);
