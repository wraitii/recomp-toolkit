// com.h - COM object and vtable emulation in guest memory.
//
// A guest COM interface pointer is the address of a 16-byte header allocated
// from the runtime heap:
//
//     +0   uint32  lpVtbl     guest address of the interface's vtable
//     +4   uint32  magic      COM_MAGIC, so a stray pointer is caught
//     +8   uint32  obj        index of the host-side ComObj
//     +12  uint32  iface      which ComIface this view is
//
// The vtable is a second guest allocation of one dword per slot, each holding
// an import trampoline address obtained from imports_alloc_trampoline(). A
// guest `call [vtbl + 4*n]` therefore lands in imports_dispatch, which runs
// the slot's shim and then pops the return address plus 4*argc bytes exactly
// as the real `ret n` would. Vtables are built once per interface and shared
// by every object of that interface, which is what the real DLLs do and what
// Task 4's parity comparison expects (it excludes COM vtable pointers).
//
// One host ComObj can be reachable through several interface views: an
// IDirectDraw2 and the IDirectDraw4 the guest gets from QueryInterface are
// two guest pointers with two vtables and one refcount, as COM requires.
// com_view() allocates a view lazily and caches it, so QueryInterface for the
// same interface twice returns the same pointer.
#pragma once
#include "../runtime/guest.h"
#include "../runtime/imports.h"
#include "dxtypes.h"

#include <array>
#include <cstring>
#include <deque>
#include <map>
#include <memory>
#include <string>
#include <utility>
#include <vector>

static const uint32_t COM_MAGIC = 0x504f504du; // 'POPM'

enum {
    COM_OFF_vtbl = 0,
    COM_OFF_magic = 4,
    COM_OFF_obj = 8,
    COM_OFF_iface = 12,
    COM_VIEW_SIZE = 16,
};

// Every interface the shims expose. The order is not significant except that
// IF_NONE is 0 and IF_COUNT bounds the per-object view table.
enum ComIface : uint16_t {
    IF_NONE = 0,
    IF_DIRECTDRAW,
    IF_DIRECTDRAW2,
    IF_DIRECTDRAW4,
    IF_DIRECTDRAW7,
    IF_DDSURFACE,
    IF_DDSURFACE2,
    IF_DDSURFACE3,
    IF_DDSURFACE4,
    IF_DDSURFACE7,
    IF_DDPALETTE,
    IF_DDCLIPPER,
    IF_DDCOLORCONTROL,
    IF_D3D,
    IF_D3D2,
    IF_D3D3,
    IF_D3D7,
    IF_D3DDEVICE7,
    IF_D3DVERTEXBUFFER7,
    IF_D3DDEVICE3,
    IF_D3DVIEWPORT3,
    IF_D3DMATERIAL3,
    IF_D3DDEVICE2,
    IF_D3DVIEWPORT2,
    IF_D3DMATERIAL2,
    IF_D3DLIGHT,
    IF_D3DTEXTURE2,
    IF_DSOUND,
    IF_DSOUND8,
    IF_DSBUFFER,
    IF_DSBUFFER8,
    IF_DS3DBUFFER,
    IF_DS3DLISTENER,
    IF_DSNOTIFY,
    IF_DINPUT,
    IF_DINPUTDEVICE,
    IF_DINPUTEFFECT,
    // DirectShow multimedia streaming (dshow.cpp)
    IF_MMSTREAM,         // IAMMultiMediaStream, and IMultiMediaStream as its prefix
    IF_MEDIASTREAM,      // IMediaStream
    IF_AUDIOMEDIASTREAM, // IAudioMediaStream
    IF_AUDIODATA,        // IAudioData, and IMemoryData as its prefix
    IF_STREAMSAMPLE,     // IAudioStreamSample, and IStreamSample as its prefix
    IF_GRAPH,            // IGraphBuilder (IFilterGraph as its prefix) of a stream
    IF_MEDIACONTROL,     // IMediaControl on that graph
    IF_MEDIAEVENT,       // IMediaEventEx (IMediaEvent as its prefix)
    IF_MEDIASEEKING,     // IMediaSeeking
    IF_BASICAUDIO,       // IBasicAudio
    IF_MEDIAPOSITION,    // IMediaPosition
    IF_ENUMFILTERS,      // IEnumFilters over the graph's (empty) filter list
    // The movie player's own filter graph (dshow.cpp): CoCreateInstance
    // (CLSID_FilterGraph) builds a real graph whose source, splitter and
    // DSound renderer are shims and whose texture renderer is the guest's.
    IF_BASEFILTER,       // IBaseFilter
    IF_PIN,              // IPin
    IF_ENUMPINS,         // IEnumPins
    IF_ENUMMEDIATETYPES, // IEnumMediaTypes
    IF_MEDIASAMPLE,      // IMediaSample handed to the guest renderer
    IF_D3D11_DEVICE,
    IF_D3D11_CONTEXT,
    IF_D3D11_TEXTURE,
    IF_D3D11_BUFFER,
    IF_D3D11_SRV,
    IF_D3D11_RTV,
    IF_D3D11_SAMPLER,
    IF_D3D11_BLEND,
    IF_D3D11_RASTER,
    IF_D3D11_VS,
    IF_D3D11_PS,
    IF_D3D11_LAYOUT,
    IF_DXGI_SWAP,
    IF_DXGI_OUTPUT,
    IF_D3D_BLOB,
    // Media Foundation (mf.cpp). A player resolves a URL into a source,
    // describes its streams, builds a topology and drives a session; the
    // interfaces no player calls are absent rather than stubbed.
    IF_MF_SOURCE_RESOLVER,
    IF_MF_MEDIA_SOURCE, // IMFMediaEventGenerator is its prefix
    IF_MF_PRESENTATION_DESCRIPTOR,
    IF_MF_STREAM_DESCRIPTOR,
    IF_MF_MEDIA_TYPE_HANDLER,
    IF_MF_MEDIA_TYPE,
    IF_MF_TOPOLOGY,
    IF_MF_TOPOLOGY_NODE,
    IF_MF_ACTIVATE,
    IF_MF_MEDIA_SESSION, // IMFMediaEventGenerator is its prefix too
    IF_MF_GET_SERVICE,   // IMFGetService, a second view of the session
    IF_MF_MEDIA_EVENT,
    IF_MF_ASYNC_RESULT,
    IF_MF_CLOCK,
    IF_MF_PRESENTATION_CLOCK, // a second view of the clock
    IF_MF_VIDEO_DISPLAY,
    IF_MF_AUDIO_VOLUME,
    // Direct3D 9 (d3d9.cpp), for a shader-era game. Nothing else in the kit
    // shares these: the version 2 interfaces above are a different object
    // model on a DirectDraw object, while IDirect3D9 stands alone.
    IF_D3D9,
    IF_D3DDEVICE9,
    IF_D3DRESOURCE9,
    IF_D3DXEFFECT33,
    IF_D3DXEFFECTPOOL33,
    IF_D3DXBUFFER,
    IF_D3DXCONSTANTTABLE,
    IF_D3DXEFFECTCOMPILER,
    // The device's resources: what a game creates to draw with.
    IF_D3DTEXTURE9,
    IF_D3DCUBETEXTURE9,
    IF_D3DSURFACE9,
    IF_D3DVERTEXBUFFER9,
    IF_D3DINDEXBUFFER9,
    IF_D3DVERTEXDECL9,
    IF_D3DQUERY9,
    IF_D3DBASETEXTURE9,
    IF_D3DVOLUME9,
    IF_D3DVOLUMETEXTURE9,
    IF_D3DVERTEXSHADER9,
    IF_D3DPIXELSHADER9,
    IF_D3DSTATEBLOCK9,
    // DirectInput 8: the same objects as DirectInput, reached through the
    // version 8 vtables.
    IF_DINPUT8,
    IF_DINPUTDEVICE8,
    // Direct3D 8 (d3d8.cpp): independent factory, device and implicit surface.
    IF_D3D8,
    IF_D3D8DEVICE,
    IF_D3D8SURFACE8,
    IF_D3D8TEXTURE8,
    IF_D3D8VERTEXBUFFER8,
    IF_D3D8INDEXBUFFER8,
    IF_CREATEERRORINFO,
    IF_ERRORINFO,
    IF_COUNT
};

// What a host object actually is. Several interfaces map to one kind.
// Direct3D has no kind of its own: IDirect3D and IDirect3D2 are interfaces on
// the DirectDraw object, exactly as in DirectX, so they share its refcount and
// its controlling IUnknown.
enum ComKind : uint16_t {
    K_NONE = 0,
    K_DDRAW,
    K_SURFACE,
    K_PALETTE,
    K_CLIPPER,
    K_D3DDEVICE,
    K_D3D7DEVICE,
    K_D3D7VB,
    K_VIEWPORT,
    K_MATERIAL,
    K_LIGHT,
    K_DSOUND,
    K_DSBUFFER,
    K_DINPUT,
    K_DIDEVICE,
    K_DINPUTEFFECT,
    K_MMSTREAM,         // a multimedia stream over one audio file
    K_MEDIASTREAM,      // its audio media stream
    K_AUDIODATA,        // a guest buffer wrapped for sampling
    K_STREAMSAMPLE,     // one sample: fills an audio data object from a stream
    K_GRAPH,            // the filter graph a multimedia stream plays through
    K_ENUMFILTERS,      // an enumerator over that graph's filters
    K_FILTERGRAPH,      // CLSID_FilterGraph: the movie player's graph
    K_BASEFILTER,       // one filter in it: source, splitter or DSound renderer
    K_PIN,              // one pin on one of those filters
    K_ENUMPINS,         // IEnumPins over a filter's pins
    K_ENUMMEDIATETYPES, // IEnumMediaTypes over a pin's media types
    K_MEDIASAMPLE,      // one frame delivered to the guest renderer
    K_D3D11_DEVICE,
    K_D3D11_CONTEXT,
    K_D3D11_TEXTURE,
    K_D3D11_BUFFER,
    K_D3D11_SRV,
    K_D3D11_RTV,
    K_D3D11_SAMPLER,
    K_D3D11_BLEND,
    K_D3D11_RASTER,
    K_D3D11_VS,
    K_D3D11_PS,
    K_D3D11_LAYOUT,
    K_DXGI_SWAP,
    K_DXGI_OUTPUT,
    K_D3D_BLOB,
    K_MF_SOURCE_RESOLVER,         // turns a URL into a media source
    K_MF_MEDIA_SOURCE,            // one open file, decoded by mf::Media
    K_MF_PRESENTATION_DESCRIPTOR, // its streams, and the duration
    K_MF_STREAM_DESCRIPTOR,       // one of those streams
    K_MF_MEDIA_TYPE_HANDLER,      // that stream's type, and its major type
    K_MF_MEDIA_TYPE,
    K_MF_TOPOLOGY,      // recorded, not resolved: the session plays the source
    K_MF_TOPOLOGY_NODE, // its nodes, for the attributes a player reads back
    K_MF_ACTIVATE,      // a renderer the player asked for by name
    K_MF_MEDIA_SESSION, // playback, and the event queue the player waits on
    K_MF_MEDIA_EVENT,
    K_MF_ASYNC_RESULT, // how one queued event reaches the player's Invoke
    K_MF_CLOCK,
    K_MF_VIDEO_DISPLAY, // IMFVideoDisplayControl on the session's renderer
    K_MF_AUDIO_VOLUME,
    K_D3D9,           // the IDirect3D9 factory object
    K_D3D9DEVICE,     // one device created from it
    K_D3DXEFFECTPOOL, // the D3DX effect pool a game shares between effects
    K_D3DXEFFECT,     // one effect loaded from the executable's resources
    K_D3DXCONSTANTTABLE,
    K_D3D9TEXTURE, // a 2D texture and its levels
    K_D3D9CUBETEXTURE,
    K_D3D9VOLUMETEXTURE,
    K_D3D9VOLUME,
    K_D3D9SURFACE,      // one surface: a texture level, a back buffer, a depth buffer
    K_D3D9VB,           // a vertex buffer
    K_D3D9IB,           // an index buffer
    K_D3D9DECL,         // a vertex declaration
    K_D3D9QUERY,        // an occlusion or event query
    K_D3D9VS,           // a vertex shader object
    K_D3D9PS,           // a pixel shader object
    K_D3D9STATEBLOCK,   // a captured or recorded device state block
    K_D3D8,             // the IDirect3D8 factory object
    K_D3D8DEVICE,       // one host wgpu device
    K_D3D8SURFACE,      // a render target / back buffer view, or one texture level
    K_D3D8TEXTURE,      // a 2D texture and its mip levels
    K_D3D8VERTEXBUFFER, // a vertex buffer's host storage
    K_D3D8INDEXBUFFER,  // an index buffer's host storage
    K_ERRORINFO,
};

// A DirectInput joystick axis's DIPROP_RANGE, DIPROP_DEADZONE and
// DIPROP_SATURATION (dx/dinput_joystick.cpp). The zone and saturation are in
// 0..10000 of the distance from centre, as DirectInput states them.
struct JoyAxisRange {
    int32_t min = 0, max = 65535;
    uint32_t deadzone = 0, saturation = 10000;
};
// One field of a custom joystick data format: the guest's offset for one
// object in dinput_joystick.h's joy_objects() list.
struct JoyFormatSlot {
    uint32_t ofs = 0;
    uint32_t object = 0;
};

// One IDirectDrawSurface4/7 private-data record, keyed by the caller's GUID.
// The bytes are a host-side copy of guest memory, like a texture's upload.
struct SurfacePrivateData {
    uint8_t guid[16] = {0};
    std::vector<uint8_t> bytes;
};

struct D3dHostDevice;
struct D3dHostStorage;
struct D3dHostResource;
struct D3dHostDevice7State;
struct D3d9HostDevice;
struct D3d9HostResource;
struct D3dxConstantTableHost;
struct D3dxEffectHost;
struct D3dxEffectPoolHost;

// ---------------------------------------------------------------------------
// The host-side object. One fat struct rather than a class hierarchy: these
// are shims, the field set is small and fixed, and a flat record keeps every
// method one field access away from what it needs.
// ---------------------------------------------------------------------------
struct ComObj {
    uint32_t id = 0;
    ComKind kind = K_NONE;
    int32_t refs = 0;
    // Parent/binding references keep storage alive without being guest-facing.
    // A ref hook can release an owner's lifetime reference when only internal
    // references remain, avoiding device -> bound resource -> device cycles.
    int32_t internal_refs = 0;
    bool alive = false;
    uint32_t views[IF_COUNT] = {0}; // guest address of each interface view
    // The first view ever handed out. COM requires QueryInterface(IID_IUnknown)
    // to return the same pointer for the lifetime of the object, so this is
    // fixed on first use and never revised when a lower-numbered view appears.
    uint32_t identity = 0;

    // --- K_DDRAW
    uint32_t hwnd = 0;
    uint32_t coop_level = 0;
    uint32_t mode_w = 0, mode_h = 0, mode_bpp = 0;
    bool mode_set = false;
    std::vector<uint32_t> surfaces; // ids, for RestoreAllSurfaces
    // Video-memory accounting. The engine prefills a texture-surface pool until
    // CreateSurface fails, so surface pixels must be bounded by the reported
    // VRAM or the pool drains the guest heap. vram_total == 0 means unbounded
    // (tests and RECOMP_DDRAW_VRAM_MB=0).
    uint64_t vram_used = 0;
    uint32_t vram_total = 0;

    // --- K_SURFACE
    uint32_t caps = 0;
    uint32_t width = 0, height = 0, bpp = 0, pitch = 0;
    // Non-zero for a DXT surface (DDPF_FOURCC). Its guest bytes are the 4x4
    // blocks themselves; bpp stays 0 as the driver reports. See dx/dxt_decode.h.
    uint32_t fourcc = 0;
    uint32_t pixels = 0; // guest address of the pixel memory
    uint32_t pixels_bytes = 0;
    bool counts_vram = false; // owner_dd's vram_used includes this surface
    uint32_t rmask = 0, gmask = 0, bmask = 0, amask = 0;
    uint32_t palette_obj = 0;
    uint32_t clipper_obj = 0;
    uint32_t back_obj = 0;            // next surface in the flip chain
    uint32_t front_obj = 0;           // the primary this back buffer belongs to
    bool implicit_backbuffer = false; // CreateSurface child: lifetime belongs to the flip chain
    uint32_t zbuffer_obj = 0;         // the Z buffer attached to this surface
    // Implicit mip children have guest-addressable storage and die with their
    // texture root. This link is separate from the flip/depth attachments.
    uint32_t mip_next = 0;
    bool mip_sublevel = false;
    // Pixels changed while this surface had no texture handle. GetHandle
    // sends them when the handle is made, so a texture that was filled before
    // anything asked for its handle is not uploaded empty.
    bool tex_dirty = false;
    uint32_t owner_dd = 0;
    uint32_t ckey_src_lo = 0, ckey_src_hi = 0;
    uint32_t ckey_dst_lo = 0, ckey_dst_hi = 0;
    bool has_ckey_src = false, has_ckey_dst = false;
    int32_t lock_count = 0;
    // Once handed a writable Lock pointer, the guest may keep using it forever.
    bool retained_pointer = false;
    bool is_primary = false;
    // True when the shim allocated `pixels` and must free them. False after
    // SetSurfaceDesc points the surface at a buffer the guest owns.
    bool owns_pixels = false;
    uint32_t texture_handle = 0; // non-zero once GetHandle was called
    uint32_t dc_handle = 0;      // pseudo HDC handed out by GetDC
    // IDirectDrawSurface4/7 private data, and the v7 priority/LOD hints.
    std::vector<SurfacePrivateData> priv_data;
    uint32_t surface_priority = 0;
    uint32_t surface_lod = 0;

    // --- K_PALETTE
    uint32_t pal_flags = 0;
    uint32_t pal[256] = {0}; // 0x00RRGGBB

    // --- K_CLIPPER
    uint32_t clip_hwnd = 0;
    std::vector<int32_t> clip_rects; // 4 LONGs each

    // --- K_D3DDEVICE
    uint32_t dev_d3d = 0;
    uint32_t bound_texture = 0;       // Device3 retains its stage-zero texture object.
    uint32_t texture_stage[32] = {0}; // Single advertised texture stage.
    uint32_t render_target = 0;       // surface id
    uint32_t current_viewport = 0;
    bool in_scene = false;
    std::vector<uint32_t> viewports;
    uint32_t render_state[D3D_RENDERSTATE_MAX] = {0};
    uint32_t light_state[D3D_LIGHTSTATE_MAX] = {0};
    float transform[D3DTRANSFORMSTATE_MAX][16] = {{0}};
    bool transform_set[D3DTRANSFORMSTATE_MAX] = {false};

    // --- K_VIEWPORT
    uint32_t vp_x = 0, vp_y = 0, vp_w = 0, vp_h = 0;
    float vp_minz = 0.0f, vp_maxz = 1.0f;
    uint32_t vp_background = 0; // material handle
    uint32_t vp_device = 0;
    std::vector<uint32_t> lights;

    // --- K_MATERIAL / K_LIGHT: the guest's own record, kept verbatim.
    std::vector<uint8_t> blob;
    uint32_t handle = 0; // material handle the guest sees

    // --- K_DSOUND
    uint32_t ds_coop = 0;
    uint32_t ds_hwnd = 0;

    // --- K_DSBUFFER
    int32_t channel = -1;
    uint32_t buf_bytes = 0;
    uint32_t buf_pixels = 0; // guest address of the PCM data
    uint32_t buf_flags = 0;
    uint32_t rate = 22050, nchannels = 1, bits = 16, block_align = 2;
    int32_t volume = 0, pan = 0;
    uint32_t frequency = 0; // 0 = the format's own rate
    bool playing = false, looping = false, is_primary_buffer = false;
    uint32_t play_cursor = 0, write_cursor = 0;
    uint32_t lock_off = 0, lock_len = 0;
    float pos3d[3] = {0, 0, 0}, vel3d[3] = {0, 0, 0};
    uint32_t notify_count = 0;
    // IDirectSound3DBuffer state. `is_3d` is set when the buffer is created
    // with DSBCAPS_CTRL3D or first exposes the 3D interface. Distances are in
    // the guest's world units; the defaults are the DS3D ones (1.0 and 1e9).
    bool is_3d = false;
    float min3d = 1.0f, max3d = 1000000000.0f;
    uint32_t mode3d = 0; // DS3DMODE_NORMAL
    uint32_t cone_inside = 360, cone_outside = 360;
    float cone_orient[3] = {0, 0, 0};
    int32_t cone_outside_volume = 0; // DSBVOLUME_MAX

    // --- K_DINPUT / K_DIDEVICE
    uint32_t di_version = 0;
    bool di_wide = false;  // created through DirectInputCreateW: DIDEVICEINSTANCEW layouts
    uint32_t dev_type = 0; // DIDEVTYPE_MOUSE / _KEYBOARD / _JOYSTICK
    uint32_t samples = 0;  // Direct3D 9 surfaces and devices: multisample count, 0 for none
    bool acquired = false;
    uint32_t di_coop = 0;
    uint32_t data_format_size = 0;
    uint32_t buffer_size = 0;  // DIPROP_BUFFERSIZE
    uint32_t notify_event = 0; // SetEventNotification, 0 when none
    uint32_t axis_mode_absolute = 0;
    uint8_t last_keys[256] = {0};
    uint8_t last_buttons[8] = {0};
    int32_t last_x = 0, last_y = 0, last_z = 0;
    uint32_t sequence = 0;
    std::vector<uint32_t> events; // packed dwOfs/dwData pairs
    // Joystick only. Ranges are per DIJOYSTATE axis slot (lX..lRz). An empty
    // format means DIJOYSTATE or DIJOYSTATE2, by data_format_size.
    JoyAxisRange joy_ranges[6];
    std::vector<JoyFormatSlot> joy_format;
    uint32_t last_sequence = 0; // the newest host pad edge already delivered

    // --- DirectShow streaming (K_MMSTREAM, K_MEDIASTREAM, K_AUDIODATA, K_STREAMSAMPLE)
    uint32_t dsh_owner = 0;        // media stream: its multimedia stream; sample: its media stream
    uint32_t dsh_stream = 0;       // multimedia stream: the audio media stream it carries
    uint32_t dsh_graph = 0;        // multimedia stream: its filter graph, once asked for
    uint32_t dsh_notify_flags = 0; // graph: IMediaEventEx::SetNotifyFlags
    uint32_t dsh_data = 0;         // sample: the audio data object it fills
    bool dsh_initialised = false;
    uint32_t dsh_state = 0;  // STREAMSTATE_STOP 0 / STREAMSTATE_RUN 1
    uint32_t dsh_pos = 0;    // an enumerator's cursor
    uint32_t dsh_filter = 0; // a pin's owning filter, or an enumerator's owning object
    uint32_t md_buffer = 0, md_length = 0, md_actual = 0; // audio data: the guest buffer
    bool md_has_format = false;
    uint32_t md_rate = 0;
    uint16_t md_channels = 0, md_bits = 0;
    uint64_t smp_start = 0, smp_end = 0; // sample: PCM frames covered by the last Update

    // IDirectDrawColorControl, per surface. The defaults are the DX6 SDK's.
    uint32_t cc_flags = 0x7f; // every field is valid
    int32_t cc_brightness = 750;
    int32_t cc_contrast = 10000;
    int32_t cc_hue = 0;
    int32_t cc_saturation = 10000;
    int32_t cc_sharpness = 5;
    int32_t cc_gamma = 1;
    int32_t cc_colorenable = 1;

    // --- K_D3D7DEVICE / K_D3D7VB. The D3D7 device's state is behind a
    // pointer so the big render-state arrays are not paid for by every COM
    // object; a vertex buffer's storage is the guest `pixels` block.
    std::shared_ptr<D3dHostDevice7State> d3d7;
    bool d3d7_native_handoff_failed = false;
    std::shared_ptr<D3dHostDevice> d3d7_host;
    uint32_t d3d7_width = 0, d3d7_height = 0;
    // --- K_SURFACE: the D3D7 device that renders into this surface, if any.
    // The guest bytes stay authoritative; this is only how ddraw.cpp finds the
    // Rust target to reconcile before it reads or presents those bytes.
    uint32_t d3d7_target_device = 0;
    uint32_t vb_fvf = 0;
    uint32_t vb_num_vertices = 0;
    uint32_t vb_caps = 0;

    std::shared_ptr<D3d9HostDevice> d3d9_device;
    uint32_t d3d9_focus_window = 0;
    std::shared_ptr<D3d9HostResource> d3d9_resource;
    bool d3d9_owner_retained = false;
    uint32_t d3d9_parent = 0; // owning texture/volume texture for a level view
    uint32_t d3d9_face = 0;
    uint32_t d3d9_level = 0;
    bool d3d9_volume = false;
    bool d3d9_parent_retained = false;
    std::vector<uint32_t> d3d9_levels; // parent texture's cached level views
    uint32_t d3d9_backbuffer = 0;
    uint32_t d3d9_depthbuffer = 0;
    bool d3d9_implicit = false;
    bool d3d9_target_depth = false;
    uint32_t d3d9_state_handle = 0;
    uint32_t d3d9_query = 0;
    std::shared_ptr<D3dxConstantTableHost> d3dx_ctab;
    uint32_t d3dx_ctab_buffer = 0;
    uint32_t d3dx_ctab_size = 0;
    uint32_t d3dx_ctab_handles = 0;
    uint32_t d3dx_ctab_nodes = 0;
    std::shared_ptr<D3dxEffectHost> d3dx_effect;
    std::shared_ptr<D3dxEffectPoolHost> d3dx_effect_pool;

    std::shared_ptr<D3dHostDevice> d3d8_device;
    std::shared_ptr<D3dHostResource> d3d8_resource;
    uint32_t d3d8_guest_backing = 0; // lifetime-owned vertex/index bytes in guest arena
    std::shared_ptr<D3dHostStorage> d3d8_storage;
    uint32_t d3d8_width = 0, d3d8_height = 0, d3d8_format = 0;
    uint32_t d3d8_factory = 0; // K_D3D8DEVICE: its IDirect3D8 object id
    uint32_t d3d8_owner = 0;   // K_D3D8SURFACE/K_D3D8TEXTURE: retained device object id
    // Weak cache of the externally held implicit backbuffer surface. The
    // surface holds its device alive; the device must not retain it in return.
    uint32_t d3d8_backbuffer = 0;
    uint32_t d3d8_present_flags = 0;
    std::array<std::array<uint32_t, 2>, 16> d3d8_streams{};
    uint32_t d3d8_lock_flags = 0;
    std::array<uint32_t, 4> d3d8_lock_rect{};
    // Weak cache of the implicit autodepth surface, materialized on demand
    // when D3DPRESENT_PARAMETERS asked for one. Same ownership rule as the
    // backbuffer: the surface retains the device, not the reverse.
    uint32_t d3d8_depthbuffer = 0;
    uint32_t d3d8_target = 0;       // internal binding reference; 0 = implicit color
    uint32_t d3d8_target_depth = 0; // internal binding reference
    bool d3d8_depth_detached = false;
    bool d3d8_owner_retained = false;
    uint32_t d3d8_depth_format = 0; // raw D3DFORMAT requested at create; 0 = none
    // --- K_D3D8TEXTURE
    std::vector<uint32_t> d3d8_levels; // K_D3D8SURFACE id per mip level
    // --- K_D3D8SURFACE texture levels (the implicit backbuffer leaves these 0)
    uint32_t d3d8_texture = 0; // owning K_D3D8TEXTURE id
    uint32_t d3d8_level = 0;   // which mip level of that texture
    // --- stream/indices state on K_D3D8DEVICE
};

// One vtable slot.
struct ComMethod {
    const char *name;
    uint8_t argc; // dwords popped, including `this`
    void (*fn)(X86 *);
};

// ---------------------------------------------------------------------------
// Lifetime
// ---------------------------------------------------------------------------
// Drops every object and vtable. Call after mem_init(), which invalidates the
// guest heap the objects live in.
void com_reset();

ComObj *com_new(ComKind kind);

extern ComObj *const *g_com_table;
extern uint32_t g_com_count;
[[gnu::cold]] ComObj *com_this_bad_magic(uint32_t addr);

inline ComObj *com_get(uint32_t id) {
    if (!id || id > g_com_count)
        return nullptr;
    ComObj *o = g_com_table[id - 1];
    return o->alive ? o : nullptr;
}
// Every object in creation order, including dead ones; the object with id n is
// at index n - 1. Prefer this over com_get when walking the whole table.
std::deque<ComObj> &com_objects();
// The object a guest interface pointer refers to, or null when `addr` is not
// one of ours. `want` may be IF_NONE to accept any interface.
inline ComObj *com_this(uint32_t addr, ComIface want = IF_NONE) {
    if (!addr || !gm_valid(addr, COM_VIEW_SIZE))
        return nullptr;
    if (rd32(addr + COM_OFF_magic) != COM_MAGIC)
        return com_this_bad_magic(addr);
    ComObj *o = com_get(rd32(addr + COM_OFF_obj));
    if (!o)
        return nullptr;
    if (want != IF_NONE && rd32(addr + COM_OFF_iface) != want)
        return nullptr;
    return o;
}
// Same, reading `this` from the shim's first stack argument.
inline ComObj *com_this_arg(X86 *c, ComIface want = IF_NONE) {
    return com_this(arg(c, 0), want);
}
ComIface com_iface_of(uint32_t addr);

// Guest address of `o` seen through `iface`, allocating the view on first use.
uint32_t com_view(ComObj *o, ComIface iface);
void com_addref(ComObj *o);
void com_set_ref_hook(ComKind kind, void (*fn)(ComObj *));
void com_internalize(ComObj *o); // convert one existing reference to internal
void com_retain_internal(ComObj *o);
void com_release_internal(ComObj *o);
// Drops one reference and destroys at zero. Returns the new count.
int32_t com_release(ComObj *o);
// Final teardown regardless of references, for an implicitly owned object
// whose parent's destruction ends its lifetime (DirectDraw flip-chain children).
void com_destroy(ComObj *o);
// A module may hook its resource teardown here.
void com_set_destructor(ComKind kind, void (*fn)(ComObj *));

// ---------------------------------------------------------------------------
// Vtables
// ---------------------------------------------------------------------------
// Builds (once) the guest vtable for `iface` and records it so com_view can
// use it. `dll` and `iface_name` name the trampolines, e.g. "DDRAW.dll" and
// "IDirectDraw2", giving "DDRAW.dll!IDirectDraw2::Blt" in the import report.
uint32_t com_define(ComIface iface, const char *dll, const char *iface_name,
                    const ComMethod *methods, size_t count);
uint32_t com_vtable_of(ComIface iface);
void com_report_failure(X86 *c, ComIface iface, size_t slot);

template <ComIface Iface, const auto &Methods, size_t Slot> void com_checked_call(X86 *c) {
    Methods[Slot].fn(c);
    if (__builtin_expect((c->r[R_EAX] & 0x80000000u) != 0, 0))
        com_report_failure(c, Iface, Slot);
}

template <ComIface Iface, const auto &Methods, size_t... Slots>
uint32_t com_define_checked(const char *dll, const char *iface_name,
                            std::index_sequence<Slots...>) {
    const ComMethod methods[] = {{Methods[Slots].name, Methods[Slots].argc,
                                  Methods[Slots].fn && Methods[Slots].fn != imports_unsupported &&
                                          strcmp(Methods[Slots].name, "GetAvailableTextureMem") != 0
                                      ? com_checked_call<Iface, Methods, Slots>
                                      : Methods[Slots].fn}...};
    return com_define(Iface, dll, iface_name, methods, std::size(methods));
}

template <ComIface Iface, const auto &Methods>
uint32_t com_define_checked(const char *dll, const char *iface_name) {
    return com_define_checked<Iface, Methods>(dll, iface_name,
                                              std::make_index_sequence<std::size(Methods)>{});
}

// Named host entries for interfaces in dx/metadata/com-methods.json. Generated
// code calls rk_host_<stem>_<Interface>_<Method> directly once runtime/imports.cpp
// has checked that the vtable trampoline still dispatches to the same function,
// so the vtable is built from these entries wherever a slot has a handler.
struct ComHostSlot {
    const char *name;
    uint8_t argc;
    void (*fn)(X86 *);
};

template <ComIface Iface, const auto &Methods, bool Checked, size_t Slot>
void com_host_entry(X86 *c) {
    if constexpr (Checked)
        com_checked_call<Iface, Methods, Slot>(c);
    else
        Methods[Slot].fn(c);
}

[[noreturn]] void com_host_mismatch(const char *dll, const char *iface_name, size_t slot,
                                    const char *what);

template <ComIface Iface, const auto &Methods, bool Checked, size_t N>
uint32_t com_define_host(const char *dll, const char *iface_name, const ComHostSlot (&host)[N]) {
    static_assert(N == std::size(Methods), "COM method table and catalog differ in slot count");
    ComMethod methods[N];
    for (size_t i = 0; i < N; ++i) {
        const ComMethod &m = Methods[i];
        if (strcmp(m.name, host[i].name) != 0)
            com_host_mismatch(dll, iface_name, i, "name differs from dx/metadata/com-methods.json");
        if (m.argc != host[i].argc)
            com_host_mismatch(dll, iface_name, i, "argc differs from dx/metadata/com-methods.json");
        bool entry = m.fn && m.fn != imports_unsupported &&
                     !(Checked && strcmp(m.name, "GetAvailableTextureMem") == 0);
        methods[i] = {m.name, m.argc, entry ? host[i].fn : m.fn};
    }
    uint32_t vt = com_define(Iface, dll, iface_name, methods, N);
    for (size_t i = 0; vt && i < N; ++i)
        if (imports_trampoline_slow(rd32(vt + 4 * (uint32_t)i)) !=
            (!Methods[i].fn || Methods[i].fn == imports_unsupported))
            com_host_mismatch(dll, iface_name, i,
                              "host entry changes the trampoline's slow-path status");
    return vt;
}

#define COM_HOST_ENTRY_(ctx, sym, slot, name, argc)                                                \
    extern "C" void sym(X86 *c) {                                                                  \
        ctx::call<slot>(c);                                                                        \
    }
#define COM_HOST_ROW_(ctx, sym, slot, name, argc) {name, argc, sym},
// Expands at global scope, after `Methods`, to the named entries for catalog
// interface `id` (e.g. d3d9_IDirect3DDevice9) and `id`_host::define(dll, name).
#define COM_HOST_SLOTS(id, Iface, Methods, Checked)                                                \
    struct id##_host {                                                                             \
        template <size_t Slot> static void call(X86 *c) {                                          \
            com_host_entry<Iface, Methods, Checked, Slot>(c);                                      \
        }                                                                                          \
        static uint32_t define(const char *dll, const char *iface_name);                           \
    };                                                                                             \
    COM_HOST_SLOTS_##id(COM_HOST_ENTRY_, id##_host) uint32_t id##_host::define(                    \
        const char *dll, const char *iface_name) {                                                 \
        static const ComHostSlot slots[] = {COM_HOST_SLOTS_##id(COM_HOST_ROW_, id##_host)};        \
        return com_define_host<Iface, Methods, Checked>(dll, iface_name, slots);                   \
    }
// Which kinds an interface may be a view of. Registering this is what makes
// QueryInterface refuse an interface the object does not implement.
void com_bind(ComIface iface, ComKind kind);
const char *com_iface_name(ComIface iface);

// ---------------------------------------------------------------------------
// Shared slot implementations. Every interface's vtable uses these for slots
// 0, 1 and 2.
// ---------------------------------------------------------------------------
void com_QueryInterface(X86 *c); // argc 3
void com_AddRef(X86 *c);         // argc 1
void com_Release(X86 *c);        // argc 1

// Registers `iid` (16 bytes) as naming `iface`. QueryInterface matches on it.
void com_register_iid(ComIface iface, const uint8_t iid[16]);
// The interface a guest GUID names, or IF_NONE.
ComIface com_iface_for_iid(uint32_t guest_guid_addr);

// A kind may redirect QueryInterface to a different object: an IDirectDraw
// asked for IID_IDirect3D2 returns the Direct3D object it owns, not itself.
// Returning null means "this object", and returning null with `handled` set
// false is the normal case.
typedef ComObj *(*ComQiHook)(ComObj *self, ComIface want);
void com_set_qi_hook(ComKind kind, ComQiHook hook);

// A kind may name interfaces that must stop the run when queried, instead of
// returning E_NOINTERFACE. This exists for an interface the original reaches
// for and dereferences without checking the result: a returned error would
// become a null dereference in the guest, which is a worse diagnostic than an
// abort that names the interface it could not get. Returning true means the
// hook handled (in practice, aborted) the query.
typedef bool (*ComQiUnsupportedHook)(ComObj *self, ComIface want);
void com_set_qi_unsupported(ComKind kind, ComQiUnsupportedHook hook);

// ---------------------------------------------------------------------------
// COM classes: what ole32's CoCreateInstance can make. A module registers the
// CLSID it serves with a constructor and the interface a request for
// IID_IUnknown gets; CoCreateInstance hands out the requested interface when
// the class's kind is bound to it and E_NOINTERFACE otherwise. Every other
// class is REGDB_E_CLASSNOTREG, which the game sees as a missing component.
// ---------------------------------------------------------------------------
void com_register_class(const uint8_t clsid[16], const char *name, ComIface primary,
                        ComObj *(*create)());
// Registers ole32.dll!CoCreateInstance. Idempotent.
void com_register_ole32();
// True when `iface` may be a view of `kind` (com_bind was called for the pair).
bool com_iface_binds(ComIface iface, ComKind kind);

// ---------------------------------------------------------------------------
// Small helpers every module needs.
// ---------------------------------------------------------------------------
// Sets EAX and logs the call at level 2.
static inline void com_ret(X86 *c, uint32_t hr) {
    set_eax(c, hr);
}

// Writes an interface pointer through a guest LPVOID*, tolerating a null
// pointer (which is a caller bug, so it reports DDERR_INVALIDPARAMS).
bool com_out_ptr(uint32_t out_addr, uint32_t value);

// ---------------------------------------------------------------------------
// Checked guest spans. Every size a shim derives from guest-supplied values
// goes through these: a 32-bit product like `stride * count` can wrap and pass
// a naive bounds check while the caller then walks the unwrapped length, so
// the multiplication is done in 64 bits and the result is range-checked before
// anything is read or written.
// ---------------------------------------------------------------------------
// True when [addr, addr + bytes) lies wholly inside the guest arena.
static inline bool gm_fits(uint32_t addr, uint64_t bytes) {
    return (uint64_t)addr + bytes <= (uint64_t)GUEST_SIZE;
}
// True when [addr, addr + count*elem) lies wholly inside the guest arena.
// count and elem are each at most 2^32-1, so the product cannot overflow 64
// bits and no separate overflow test is needed.
static inline bool gm_fits_n(uint32_t addr, uint64_t count, uint64_t elem) {
    return gm_fits(addr, count * elem);
}
// The byte size of a w*h*bytes_per_pixel image, or 0 when it would exceed the
// arena. Callers treat 0 as "refuse this surface".
static inline uint64_t gm_image_bytes(uint64_t pitch, uint64_t height) {
    uint64_t n = pitch * height;
    return n <= (uint64_t)GUEST_SIZE ? n : 0;
}

// Zeroes `n` bytes of guest memory.
void gm_zero(uint32_t addr, uint32_t n);
// Copies within guest memory.
void gm_copy(uint32_t dst, uint32_t src, uint32_t n);

// A slot that is part of the interface but does nothing here. It logs once
// under its trampoline name and returns `hr`. Used through the COM_STUB macro
// so every unimplemented slot still has the right argc and a name in the
// import coverage report.
void com_stub_ok(X86 *c);
void com_stub_notimpl(X86 *c);

// Declares a named slot body that logs once and returns `hr`. Preferred over
// com_stub_ok/com_stub_notimpl wherever knowing which method the guest called
// would help, because a shim cannot discover its own trampoline name.
#define DX_STUB(name, hr)                                                                          \
    static void name(X86 *c) {                                                                     \
        log_once("dx." #name, "dx: " #name " is not implemented; returning " #hr);                 \
        set_eax(c, (uint32_t)(hr));                                                                \
    }

// Total live objects, for the tests and for leak reporting.
uint32_t com_live_count();
// Highest object id ever handed out. Ids run 1..com_object_count(); com_get
// returns null for the ones already destroyed, so this bounds a scan over
// every object that still exists.
uint32_t com_object_count();
void com_dump(FILE *out);
