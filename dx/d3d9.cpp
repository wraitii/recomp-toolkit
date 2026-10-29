#include "adapter_profiles.h"
#include "com.h"
#include "dx.h"
#include "../runtime/guest.h"
#include "../runtime/memory.h"
#include "host_api.h"
#include "frame_dump.h"
#include "d3d9_constants.inc"
#include "d3d_bridge_helpers.h"
#include "d3dx9shader_shim.h"
#include "d3dx9effect_shim.h"
#include "d3dx9tex.h"
#include "../runtime/win32.h"
#include "../platform/os.h"
#include "../runtime/display_seam.h"
#include <unordered_map>
#include <map>
#include <set>

#include <string.h>
#include <iterator>
#include <algorithm>
#include <initializer_list>
#include <cstdlib>
#include <cmath>

#define MAKE_D3DHRESULT(code) (0x88760000u | (uint32_t)(code))
static const uint32_t D3D_OK9 = 0u;
static const uint32_t D3DERR_NOTAVAILABLE = MAKE_D3DHRESULT(2154);
static const uint32_t D3DERR_INVALIDCALL = MAKE_D3DHRESULT(2156);
static const uint32_t D3DERR_NOTFOUND = MAKE_D3DHRESULT(2150);
static const uint32_t D3DERR_MOREDATA = MAKE_D3DHRESULT(2151);
static const uint32_t D3DERR_DEVICELOST = MAKE_D3DHRESULT(2152);
static const uint32_t D3DERR_DEVICENOTRESET = MAKE_D3DHRESULT(2153);
static const uint32_t D3DOK_NOAUTOGEN = 0x08760000u | 2159u;
static constexpr uint32_t D3D9_PIXEL_SHADER_FLOAT_CONSTANTS = 224;
static constexpr uint32_t D3D9_LOCK_READONLY = 0x10u;

static uint32_t bridge_result(X86 *c, const recomp::d3d::Status &status) {
    if (status.code == recomp::d3d::StatusCode::Ok)
        return D3D_OK9;
    std::string message(status.message.data(), status.message.size());
    const bool fatal = status.code == recomp::d3d::StatusCode::Unsupported ||
                       status.code == recomp::d3d::StatusCode::NotImplemented;
    if (status.code == recomp::d3d::StatusCode::NotAvailable)
        log_once(("d3d9:not-available:" + message).c_str(), "d3d9: %s", message.c_str());
    else if (fatal)
        LOGW("d3d9: aborting on unsupported renderer result: %s", message.c_str());
    else
        LOGW("d3d9: %s", message.c_str());
    if (fatal) {
        fflush(stderr);
        imports_unsupported(c);
    }
    if (status.code == recomp::d3d::StatusCode::InvalidArgument)
        return D3DERR_INVALIDCALL;
    if (status.code == recomp::d3d::StatusCode::InvalidData)
        return MAKE_D3DHRESULT(2905);
    if (status.code == recomp::d3d::StatusCode::OutOfMemory)
        return E_OUTOFMEMORY;
    if (status.code == recomp::d3d::StatusCode::WasStillDrawing)
        return MAKE_D3DHRESULT(540);
    if (status.code == recomp::d3d::StatusCode::DeviceLost)
        return D3DERR_DEVICELOST;
    if (status.code == recomp::d3d::StatusCode::DeviceNotReset)
        return D3DERR_DEVICENOTRESET;
    if (status.code == recomp::d3d::StatusCode::NoAutogen)
        return D3DOK_NOAUTOGEN;
    if (status.code == recomp::d3d::StatusCode::NotAvailable)
        return D3DERR_NOTAVAILABLE;
    return D3DERR_NOTAVAILABLE;
}

static void d3d9_setup_guest_fpu(X86 *c, uint32_t behavior_flags) {
    if (!(behavior_flags & 0x2u))
        x87_set_cw(c, (uint16_t)((c->fpu_cw & ~0x0f3fu) | 0x003fu));
}

// Defined with the resources further down.
struct ComObj;
static ComObj *device_backbuffer(ComObj *dev);
static ComObj *device_depthbuffer(ComObj *dev);
static void device_discard_implicit_surfaces(ComObj *dev);
static bool implicit_surface_externally_referenced(uint32_t field);
static void create_texture_wgpu(X86 *c, recomp::d3d::D3d9TextureKind kind);
static void create_surface_wgpu(X86 *c, uint32_t out_index, uint32_t usage);
static ComObj *com_level_view(ComObj *tex, uint32_t face, uint32_t level, bool volume);
static void level_lock_wgpu(X86 *c, ComObj *o, uint32_t face, uint32_t level, bool volume,
                            uint32_t rect_addr, uint32_t box_addr, uint32_t flags, uint32_t out,
                            bool box_output);
static void level_unlock_wgpu(X86 *c, ComObj *o, uint32_t face, uint32_t level);
static ComObj *level_owner(ComObj *o, uint32_t *face, uint32_t *level, bool *volume);

// One scratch block for structures a method fills in for the guest.
static uint32_t g_scratch = 0, g_scratch_size = 0;
static float d3dx_rf(uint32_t addr);
static void d3dx_wf(uint32_t addr, float value);

static ComObj *this_d3d9(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_D3D9) ? o : nullptr;
}
static ComObj *this_device9(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_D3D9DEVICE) ? o : nullptr;
}

static bool d3d9_flush_resource_owner(X86 *c, ComObj *resource) {
    ComObj *device = resource ? com_get(resource->dev_d3d) : nullptr;
    if (!device || !device->d3d9_device)
        return true;
    recomp::d3d::Status status = device->d3d9_device->flush();
    if (status.code == recomp::d3d::StatusCode::Ok)
        return true;
    com_ret(c, bridge_result(c, status));
    return false;
}

static uint32_t g_d3d9_display_owner = 0;

static void d3d9_clear_virtual_display(ComObj *dev) {
    if (!dev || g_d3d9_display_owner != dev->id)
        return;
    g_d3d9_display_owner = 0;
    uint32_t w = 0, h = 0, bpp = 0;
    win32_display_mode(&w, &h, &bpp);
    host_set_display_mode((int)w, (int)h, (int)bpp);
}

static void d3d9_sync_virtual_display(X86 *c, ComObj *dev, uint32_t hwnd) {
    if (!dev || !dev->d3d9_device)
        return;
    recomp::d3d::D3d9PresentParams present = dev->d3d9_device->effective_present();
    if (present.windowed != 0) {
        d3d9_clear_virtual_display(dev);
        if (hwnd)
            win32_uncover_display(c, hwnd);
        return;
    }
    g_d3d9_display_owner = dev->id;
    host_set_display_mode((int)present.width, (int)present.height, 32);
    if (hwnd)
        win32_cover_display(c, hwnd, present.width, present.height);
}

static bool fetch_presented(uint32_t owner, uint32_t *argb, int w, int h) {
    ComObj *dev = com_get(owner);
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device || w <= 0 || h <= 0) {
        log_once("d3d9.fetch.device", "d3d9: GDI refresh has no presentable D3D9 device");
        return false;
    }
    recomp::d3d::D3d9FrameResult result = dev->d3d9_device->present_readback();
    if (result.status.code != recomp::d3d::StatusCode::Ok || result.width != uint32_t(w) ||
        result.height != uint32_t(h) || result.bytes.size() != size_t(w) * h * 4) {
        std::string message(result.status.message.data(), result.status.message.size());
        log_once("d3d9.fetch.readback",
                 "d3d9: GDI could not read back the presented frame (%ux%u): %s", result.width,
                 result.height, message.c_str());
        return false;
    }
    const uint8_t *src = result.bytes.data();
    for (size_t i = 0; i < size_t(w) * h; ++i)
        argb[i] = 0xff000000u | uint32_t(src[i * 4]) << 16 | uint32_t(src[i * 4 + 1]) << 8 |
                  uint32_t(src[i * 4 + 2]);
    return true;
}

static bool read_guest_rect(uint32_t addr, recomp::d3d::D3d9Rect *out) {
    if (!addr || !gm_valid(addr, 16))
        return false;
    out->left = (int32_t)rd32(addr);
    out->top = (int32_t)rd32(addr + 4);
    out->right = (int32_t)rd32(addr + 8);
    out->bottom = (int32_t)rd32(addr + 12);
    return true;
}

static uint32_t d3d9_target_id(ComObj *s) {
    if (!s)
        return 0;
    if (s->d3d9_implicit)
        return s->d3d9_target_depth ? 0xffffffffu : 0u;
    return s->id;
}

static void return_surface_handle(X86 *c, ComObj *s, uint32_t out) {
    if (!s) {
        com_ret(c, D3DERR_NOTFOUND);
        return;
    }
    com_addref(s);
    uint32_t view_ = com_view(s, IF_D3DSURFACE9);
    if (!view_) {
        com_release(s);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view_);
    com_ret(c, D3D_OK9);
}

static void d3d9_ref_add(uint32_t id) {
    if (ComObj *o = com_get(id))
        com_retain_internal(o);
}
static void d3d9_ref_del(uint32_t id) {
    if (ComObj *o = com_get(id))
        com_release_internal(o);
}
static std::array<uint32_t, 38> d3d9_binding_ids(ComObj *dev,
                                                 const recomp::d3d::D3d9Bindings &bindings) {
    std::array<uint32_t, 38> out{};
    std::copy(bindings.textures.begin(), bindings.textures.end(), out.begin());
    std::copy(bindings.streams.begin(), bindings.streams.end(), out.begin() + 16);
    out[32] = bindings.indices;
    auto resolve = [&](const std::unordered_map<uint32_t, uint32_t> &map, uint32_t handle) {
        if (!handle)
            return 0u;
        auto it = map.find(handle);
        return it == map.end() ? 0u : it->second;
    };
    out[33] = resolve(dev->d3d9_device->shader_views, bindings.vertex_shader);
    out[34] = resolve(dev->d3d9_device->shader_views, bindings.pixel_shader);
    out[35] = resolve(dev->d3d9_device->decl_views, bindings.declaration);
    return out;
}
static std::set<uint32_t> d3d9_binding_set(ComObj *dev, const recomp::d3d::D3d9Bindings &bindings) {
    std::set<uint32_t> out;
    for (uint32_t id : d3d9_binding_ids(dev, bindings))
        if (id)
            out.insert(id);
    return out;
}
static void d3d9_reconcile(std::set<uint32_t> &current, const std::set<uint32_t> &desired) {
    for (uint32_t id : current)
        if (!desired.count(id))
            d3d9_ref_del(id);
    for (uint32_t id : desired)
        if (!current.count(id))
            d3d9_ref_add(id);
    current = desired;
}
static void d3d9_reconcile_bindings(std::array<uint32_t, 38> &snapshot,
                                    const std::array<uint32_t, 38> &ids) {
    if (snapshot == ids)
        return;
    std::array<uint32_t, 38> released;
    size_t count = 0;
    for (size_t i = 0; i < ids.size(); ++i) {
        if (snapshot[i] == ids[i])
            continue;
        if (ids[i])
            d3d9_ref_add(ids[i]);
        if (snapshot[i])
            released[count++] = snapshot[i];
    }
    for (size_t i = 0; i < count; ++i)
        d3d9_ref_del(released[i]);
    snapshot = ids;
}
static void d3d9_refresh_bindings(ComObj *dev) {
    if (!dev || !dev->d3d9_device)
        return;
    recomp::d3d::D3d9BindingsResult result = dev->d3d9_device->current_bindings();
    if (result.status.code != recomp::d3d::StatusCode::Ok)
        return;
    auto *host = dev->d3d9_device.get();
    auto live = d3d9_binding_ids(dev, result.live);
    size_t slot = 36;
    for (const auto &target : {host->render_target(), host->depth_stencil()}) {
        if (target.status.code == recomp::d3d::StatusCode::Ok && target.value &&
            target.value != 0xffffffffu)
            live[slot] = target.value;
        ++slot;
    }
    auto recording = result.recording_active ? d3d9_binding_ids(dev, result.recording)
                                             : std::array<uint32_t, 38>{};
    d3d9_reconcile_bindings(host->live_bindings, live);
    d3d9_reconcile_bindings(host->recording_bindings, recording);
}
static void d3d9_bind_slot(ComObj *dev, size_t slot, uint32_t id) {
    D3d9HostDevice *host = dev->d3d9_device.get();
    if (host->recording_block) {
        d3d9_refresh_bindings(dev);
        return;
    }
    uint32_t &current = host->live_bindings[slot];
    if (current == id)
        return;
    if (id)
        d3d9_ref_add(id);
    if (current)
        d3d9_ref_del(current);
    current = id;
}
// Retail D3D9 does not refuse draws from a locked buffer; the device reads
// whatever the guest has written so far, as with a managed buffer's dirty upload.
static bool d3d9_sync_locked_buffers(X86 *c, ComObj *dev) {
    for (size_t slot = 16; slot <= 32; ++slot) {
        ComObj *o = com_get(dev->d3d9_device->live_bindings[slot]);
        if (!o || !o->d3d9_resource)
            continue;
        auto it = o->d3d9_resource->staging.find(0);
        if (it == o->d3d9_resource->staging.end() || !it->second.active || !it->second.writable)
            continue;
        recomp::d3d::Status status = o->d3d9_resource->sync_locked(
            rust::Slice<const uint8_t>(gm_ptr(it->second.addr), it->second.size));
        if (status.code != recomp::d3d::StatusCode::Ok) {
            com_ret(c, bridge_result(c, status));
            return false;
        }
    }
    return true;
}
static void d3d9_refresh_block_bindings(ComObj *dev, uint32_t handle) {
    if (!dev || !dev->d3d9_device)
        return;
    recomp::d3d::D3d9BlockBindingsResult result = dev->d3d9_device->state_block_bindings(handle);
    std::set<uint32_t> &refs = dev->d3d9_device->block_refs[handle];
    if (result.status.code == recomp::d3d::StatusCode::Ok)
        d3d9_reconcile(refs, d3d9_binding_set(dev, result.bindings));
}
static void d3d9_drop_block_refs(ComObj *dev, uint32_t handle) {
    if (!dev || !dev->d3d9_device)
        return;
    auto it = dev->d3d9_device->block_refs.find(handle);
    if (it == dev->d3d9_device->block_refs.end())
        return;
    for (uint32_t id : it->second)
        d3d9_ref_del(id);
    dev->d3d9_device->block_refs.erase(it);
}
static void d3d9_drop_all_refs(ComObj *dev) {
    if (!dev || !dev->d3d9_device)
        return;
    D3d9HostDevice *host = dev->d3d9_device.get();
    for (uint32_t id : host->live_bindings)
        if (id)
            d3d9_ref_del(id);
    for (uint32_t id : host->recording_bindings)
        if (id)
            d3d9_ref_del(id);
    host->live_bindings.fill(0);
    host->recording_bindings.fill(0);
    for (auto &entry : host->block_refs)
        for (uint32_t id : entry.second)
            d3d9_ref_del(id);
    host->block_refs.clear();
}
static ComObj *d3d9_make_object(ComObj *dev, ComKind kind, ComIface iface, uint32_t handle,
                                uint32_t out) {
    ComObj *o = com_new(kind);
    if (!o)
        return nullptr;
    o->dev_d3d = dev->id;
    o->d3d9_state_handle = handle;
    com_retain_internal(dev);
    o->d3d9_owner_retained = true;
    uint32_t view = com_view(o, iface);
    if (!view) {
        com_release(o);
        return nullptr;
    }
    com_out_ptr(out, view);
    return o;
}

// ---------------------------------------------------------------------------
// IDirect3D9
// ---------------------------------------------------------------------------
// One adapter, the host window's display. A game that enumerates further gets
// an honest "there is only one".
void D9_RegisterSoftwareDevice(X86 *c) {
    com_ret(c, D3DERR_NOTAVAILABLE);
}
void D9_GetAdapterCount(X86 *c) {
    set_eax(c, 1);
}

// D3DADAPTER_IDENTIFIER9: two 512-byte strings, a 32-byte device name, the
// driver version, four ids, the identifier GUID and the WHQL level.
void D9_GetAdapterIdentifier(X86 *c) {
    uint32_t out = arg(c, 3);
    if (!out || !this_d3d9(c) || arg(c, 1) != 0 || !gm_valid(out, 1100)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9AdapterIdentifierResult result =
        recomp::d3d::d3d9_adapter_identifier(arg(c, 1));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    memset(gm_ptr(out), 0, 1100);
    std::string description(result.description.data(), result.description.size());
    const AdapterProfile *profile = adapter_profile();
    gm_put_str(out, profile ? profile->driver : "wgpu", 512);
    gm_put_str(out + 512, profile ? profile->description : description.c_str(), 512);
    gm_put_str(out + 1024, "\\\\.\\DISPLAY1", 32);
    wr32(out + 1056, profile ? profile->driver_version_low : result.driver_version_low);
    wr32(out + 1060, profile ? profile->driver_version_high : result.driver_version_high);
    wr32(out + 1064, profile ? profile->vendor_id : result.vendor_id);
    wr32(out + 1068, profile ? profile->device_id : result.device_id);
    wr32(out + 1072, result.sub_sys_id);
    wr32(out + 1076, result.revision);
    wr32(out + 1096, result.whql_level);
    com_ret(c, D3D_OK9);
}

static uint32_t d3d9_display_format(uint32_t bpp) {
    return bpp == 32 ? 22u : bpp == 16 ? 23u : 0u;
}
static recomp::d3d::D3d9DisplayMode d3d9_current_display_mode() {
    uint32_t w = 0, h = 0, bpp = 0;
    win32_display_mode(&w, &h, &bpp);
    recomp::d3d::D3d9DisplayMode mode{};
    mode.width = w;
    mode.height = h;
    mode.refresh_rate = (uint32_t)std::lround(host_display_refresh_rate());
    mode.format = d3d9_display_format(bpp);
    return mode;
}
static uint32_t desktop_format9() {
    return d3d9_current_display_mode().format;
}

static void add_display_mode(std::vector<recomp::d3d::D3d9DisplayMode> &modes, uint32_t w,
                             uint32_t h) {
    if (!w || !h)
        return;
    for (const auto &m : modes)
        if (m.width == w && m.height == h)
            return;
    recomp::d3d::D3d9DisplayMode mode{};
    mode.width = w;
    mode.height = h;
    mode.refresh_rate = (uint32_t)std::lround(host_display_refresh_rate());
    mode.format = d3d9_display_format(32);
    modes.push_back(mode);
}

static std::vector<recomp::d3d::D3d9DisplayMode> d3d9_display_modes() {
    std::vector<recomp::d3d::D3d9DisplayMode> modes;
    static const uint32_t kStandard[][2] = {
        {640, 480},   {800, 600},   {1024, 768},  {1152, 864},  {1280, 720},  {1280, 800},
        {1280, 960},  {1280, 1024}, {1366, 768},  {1400, 1050}, {1440, 900},  {1600, 900},
        {1600, 1200}, {1680, 1050}, {1920, 1080}, {1920, 1200}, {1920, 1440}, {2048, 1536},
        {2560, 1440}, {2560, 1600}, {2560, 1920},
    };
    for (const auto &m : kStandard)
        add_display_mode(modes, m[0], m[1]);
    int32_t host_w = 0, host_h = 0;
    if (host_display_screen(&host_w, &host_h) && host_w > 0 && host_h > 0)
        add_display_mode(modes, (uint32_t)host_w, (uint32_t)host_h);
    return modes;
}

static recomp::d3d::D3d9DisplayMode
d3d9_device_display_mode(const recomp::d3d::D3d9PresentParams &present) {
    if (present.windowed != 0 || !present.width || !present.height)
        return d3d9_current_display_mode();
    recomp::d3d::D3d9DisplayMode mode{};
    mode.width = present.width;
    mode.height = present.height;
    mode.refresh_rate = present.fullscreen_refresh_rate
                            ? present.fullscreen_refresh_rate
                            : (uint32_t)std::lround(host_display_refresh_rate());
    mode.format = d3d9_display_format(32);
    return mode;
}

static void put_mode(uint32_t p, const recomp::d3d::D3d9DisplayMode &mode) {
    if (!p)
        return;
    wr32(p + 0, mode.width);
    wr32(p + 4, mode.height);
    wr32(p + 8, mode.refresh_rate);
    wr32(p + 12, mode.format);
}

void D9_GetAdapterModeCount(X86 *c) {
    if (!this_d3d9(c) || arg(c, 1) != 0 || arg(c, 2) != d3d9_display_format(32)) {
        set_eax(c, 0);
        return;
    }
    set_eax(c, (uint32_t)d3d9_display_modes().size());
}

static void put_display_mode(uint32_t p) {
    if (!p)
        return;
    put_mode(p, d3d9_current_display_mode());
}
void D9_EnumAdapterModes(X86 *c) {
    ComObj *d3d = this_d3d9(c);
    uint32_t out = arg(c, 4);
    uint32_t format = d3d9_display_format(32);
    if (!d3d || arg(c, 1) != 0 || !out || !gm_valid(out, 16) || arg(c, 2) != format) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<recomp::d3d::D3d9DisplayMode> modes = d3d9_display_modes();
    uint32_t mode = arg(c, 3);
    if (mode >= modes.size()) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    put_mode(out, modes[mode]);
    com_ret(c, D3D_OK9);
}
void D9_GetAdapterDisplayMode(X86 *c) {
    ComObj *d3d = this_d3d9(c);
    uint32_t out = arg(c, 2);
    if (!d3d || arg(c, 1) != 0 || !out || !gm_valid(out, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (desktop_format9() == 0) {
        com_ret(c, D3DERR_NOTAVAILABLE);
        return;
    }
    put_display_mode(out);
    com_ret(c, D3D_OK9);
}
void D9_CheckDeviceType(X86 *c) {
    if (!this_d3d9(c)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CheckDeviceTypeRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.adapter_format = arg(c, 3);
    request.backbuffer_format = arg(c, 4);
    request.windowed = arg(c, 5);
    com_ret(c, bridge_result(c, recomp::d3d::d3d9_check_device_type(request)));
}
void D9_CheckDeviceFormat(X86 *c) {
    if (!this_d3d9(c)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CheckDeviceFormatRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.adapter_format = arg(c, 3);
    request.usage = arg(c, 4);
    request.resource_type = arg(c, 5);
    request.check_format = arg(c, 6);
    com_ret(c, bridge_result(c, recomp::d3d::d3d9_check_device_format(request)));
}
// D3DMULTISAMPLE_TYPE and quality as a sample count: 2x and 4x, which every
// Apple GPU renders. NONMASKABLE (1) offers them as quality levels 0 and 1.
static uint32_t multisample_count(uint32_t type, uint32_t quality) {
    if (type == 1)
        return quality == 0 ? 2 : 4;
    if (type >= 2)
        return type >= 4 ? 4 : 2;
    return 0;
}
// (this, Adapter, DeviceType, SurfaceFormat, Windowed, MultiSampleType,
// pQualityLevels)
void D9_CheckDeviceMultiSampleType(X86 *c) {
    uint32_t quality = arg(c, 6);
    if (!this_d3d9(c) || (quality && !gm_valid(quality, 4))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CheckMultiSampleRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.surface_format = arg(c, 3);
    request.windowed = arg(c, 4);
    request.multisample_type = arg(c, 5);
    recomp::d3d::ValueResult result = recomp::d3d::d3d9_check_device_multisample_type(request);
    if (result.status.code == recomp::d3d::StatusCode::Ok && quality)
        wr32(quality, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void D9_CheckDepthStencilMatch(X86 *c) {
    if (!this_d3d9(c)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CheckDepthStencilRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.adapter_format = arg(c, 3);
    request.render_target_format = arg(c, 4);
    request.depth_stencil_format = arg(c, 5);
    com_ret(c, bridge_result(c, recomp::d3d::d3d9_check_depth_stencil_match(request)));
}
void D9_CheckDeviceFormatConversion(X86 *c) {
    if (!this_d3d9(c)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CheckFormatConversionRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.source_format = arg(c, 3);
    request.destination_format = arg(c, 4);
    com_ret(c, bridge_result(c, recomp::d3d::d3d9_check_device_format_conversion(request)));
}

void D9_GetDeviceCaps(X86 *c) {
    uint32_t out = arg(c, 3);
    if (!this_d3d9(c) || arg(c, 1) != 0 || arg(c, 2) < D3DDEVTYPE_HAL || arg(c, 2) > 3 || !out ||
        !gm_valid(out, 304)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (arg(c, 2) != D3DDEVTYPE_HAL) {
        com_ret(c, D3DERR_NOTAVAILABLE);
        return;
    }
    recomp::d3d::D3d9CapsResult result = recomp::d3d::d3d9_adapter_caps(arg(c, 1));
    if (result.status.code != recomp::d3d::StatusCode::Ok || result.caps.size() != 304) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    memcpy(gm_ptr(out), result.caps.data(), 304);
    com_ret(c, D3D_OK9);
}
void D9_GetAdapterMonitor(X86 *c) {
    if (!this_d3d9(c) || arg(c, 1) != 0) {
        set_eax(c, 0);
        return;
    }
    set_eax(c, 1); // a non-null HMONITOR
}

void D9_CreateDevice(X86 *c) {
    ComObj *d3d = this_d3d9(c);
    uint32_t present = arg(c, 5), out = arg(c, 6);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    if (!d3d) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!present || !gm_valid(present, 56)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9DeviceRequest request{};
    request.adapter = arg(c, 1);
    request.device_type = arg(c, 2);
    request.behavior_flags = arg(c, 4);
    request.present.width = rd32(present + 0);
    request.present.height = rd32(present + 4);
    request.present.format = rd32(present + 8);
    request.present.count = rd32(present + 12);
    request.present.multisample_type = rd32(present + 16);
    request.present.multisample_quality = rd32(present + 20);
    request.present.swap_effect = rd32(present + 24);
    request.present.windowed = rd32(present + 32);
    request.present.enable_auto_depth_stencil = rd32(present + 36);
    request.present.auto_depth_stencil_format = rd32(present + 40);
    request.present.flags = rd32(present + 44);
    request.present.fullscreen_refresh_rate = rd32(present + 48);
    request.present.presentation_interval = rd32(present + 52);
    uint32_t focus_window = arg(c, 3);
    uint32_t device_window = rd32(present + 28);
    if (!device_window)
        device_window = focus_window;
    if (focus_window && !host_window_rect(focus_window, nullptr, nullptr, nullptr, nullptr)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (request.present.windowed) {
        int32_t w = 0, h = 0;
        if (!device_window || !host_window_rect(device_window, nullptr, nullptr, &w, &h) ||
            w <= 0 || h <= 0) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        if (!request.present.width)
            request.present.width = (uint32_t)w;
        if (!request.present.height)
            request.present.height = (uint32_t)h;
    } else if (!focus_window || !win32_top_level(focus_window) || !device_window ||
               !win32_top_level(device_window)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    request.display_mode = d3d9_device_display_mode(request.present);
    ComObj *dev = com_new(K_D3D9DEVICE);
    if (!dev) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->dev_d3d = d3d->id;
    com_addref(d3d);
    dev->d3d9_focus_window = focus_window;
    dev->hwnd = device_window;
    dev->d3d9_device = std::make_shared<D3d9HostDevice>();
    uint16_t previous_cw = c->fpu_cw;
    d3d9_setup_guest_fpu(c, request.behavior_flags);
    LOGW("d3d9: CreateDevice windowed=%u %ux%u fmt=%u count=%u msaa=%u/%u swap=%u flags=0x%x "
         "refresh=%u interval=0x%x behavior=0x%x desktop=%ux%u fmt=%u FPU CW %04x -> %04x",
         request.present.windowed, request.present.width, request.present.height,
         request.present.format, request.present.count, request.present.multisample_type,
         request.present.multisample_quality, request.present.swap_effect, request.present.flags,
         request.present.fullscreen_refresh_rate, request.present.presentation_interval,
         request.behavior_flags, request.display_mode.width, request.display_mode.height,
         request.display_mode.format, previous_cw, c->fpu_cw);
    recomp::d3d::D3d9CreateResult result =
        recomp::d3d::d3d9_create_device(*dev->d3d9_device->value, request);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_release(dev);
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t view = com_view(dev, IF_D3DDEVICE9);
    if (!view) {
        com_release(dev);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(present + 0, result.present.width);
    wr32(present + 4, result.present.height);
    wr32(present + 8, result.present.format);
    wr32(present + 12, result.present.count);
    wr32(present + 16, result.present.multisample_type);
    wr32(present + 20, result.present.multisample_quality);
    wr32(present + 24, result.present.swap_effect);
    wr32(present + 36, result.present.enable_auto_depth_stencil);
    wr32(present + 40, result.present.auto_depth_stencil_format);
    wr32(present + 44, result.present.flags);
    wr32(present + 48, result.present.fullscreen_refresh_rate);
    wr32(present + 52, result.present.presentation_interval);
    dev->width = result.present.width;
    dev->height = result.present.height;
    dev->bpp = 32;
    d3d9_sync_virtual_display(c, dev, dev->hwnd);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

static ComObj *device_backbuffer(ComObj *dev);
static ComObj *device_depthbuffer(ComObj *dev);

// DIVERGENCE(original): accept gamma updates without changing host output.
void Dev_SetGammaRamp(X86 *c) {
    log_once("d3d9.device.SetGammaRamp",
             "d3d9: SetGammaRamp ignored; gamma correction is not implemented");
    com_ret(c, 0);
}

// (this, pRect)
void Dev_SetScissorRect(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t r = arg(c, 1);
    if (!dev || !dev->d3d9_device || !r || !gm_valid(r, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(
                   c, dev->d3d9_device->set_scissor((int32_t)rd32(r), (int32_t)rd32(r + 4),
                                                    (int32_t)rd32(r + 8), (int32_t)rd32(r + 12))));
}

static ComObj *device_backbuffer(ComObj *dev);
static ComObj *device_depthbuffer(ComObj *dev);

static void d3d9_reset_transaction(ComObj *dev, const recomp::d3d::D3d9ResetRequest &request,
                                   recomp::d3d::D3d9ResetResult &result) {
    result = dev->d3d9_device->reset(request);
    if (result.status.code != recomp::d3d::StatusCode::Ok)
        return;
    recomp::d3d::ValuesResult ids = dev->d3d9_device->registered_resource_ids();
    if (ids.status.code != recomp::d3d::StatusCode::Ok) {
        dev->d3d9_device->abort_reset();
        result.status = ids.status;
        return;
    }
    for (uint32_t id : ids.values) {
        ComObj *o = com_get(id);
        if (!o || !o->d3d9_resource || o->d3d9_parent || o->d3d9_implicit ||
            o->dev_d3d != dev->id) {
            dev->d3d9_device->abort_reset();
            LOGW("d3d9: reset holder %u is not a root resource of this device", id);
            result.status.code = recomp::d3d::StatusCode::InvalidArgument;
            return;
        }
    }
    for (uint32_t id : ids.values) {
        ComObj *o = com_get(id);
        recomp::d3d::Status prepared =
            dev->d3d9_device->prepare_resource_reset(o->id, *o->d3d9_resource);
        if (prepared.code != recomp::d3d::StatusCode::Ok) {
            dev->d3d9_device->abort_reset();
            result.status = prepared;
            return;
        }
    }
    recomp::d3d::Status ready = dev->d3d9_device->reset_ready();
    if (ready.code != recomp::d3d::StatusCode::Ok) {
        dev->d3d9_device->abort_reset();
        result.status = ready;
        return;
    }
    for (uint32_t id : ids.values)
        dev->d3d9_device->commit_resource_reset(id, *com_get(id)->d3d9_resource);
    result = dev->d3d9_device->finish_reset();
    d3d9_refresh_bindings(dev);
}

void Dev_Reset(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t present = arg(c, 1);
    if (!dev || !present) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device || !gm_valid(present, 56)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9ResetRequest request{};
    request.present.width = rd32(present + 0);
    request.present.height = rd32(present + 4);
    request.present.format = rd32(present + 8);
    request.present.count = rd32(present + 12);
    request.present.multisample_type = rd32(present + 16);
    request.present.multisample_quality = rd32(present + 20);
    request.present.swap_effect = rd32(present + 24);
    request.present.windowed = rd32(present + 32);
    request.present.enable_auto_depth_stencil = rd32(present + 36);
    request.present.auto_depth_stencil_format = rd32(present + 40);
    request.present.flags = rd32(present + 44);
    request.present.fullscreen_refresh_rate = rd32(present + 48);
    request.present.presentation_interval = rd32(present + 52);
    uint32_t focus_window = dev->d3d9_focus_window;
    uint32_t device_window = rd32(present + 28);
    if (!device_window)
        device_window = focus_window;
    bool windowed = request.present.windowed != 0;
    request.window_valid = false;
    if (windowed) {
        int32_t w = 0, h = 0;
        if (device_window && host_window_rect(device_window, nullptr, nullptr, &w, &h) && w > 0 &&
            h > 0) {
            if (!request.present.width)
                request.present.width = (uint32_t)w;
            if (!request.present.height)
                request.present.height = (uint32_t)h;
            request.window_valid = true;
        }
    } else {
        request.window_valid = focus_window && win32_top_level(focus_window) && device_window &&
                               win32_top_level(device_window);
    }
    request.implicit_external_refs = implicit_surface_externally_referenced(dev->d3d9_backbuffer) ||
                                     implicit_surface_externally_referenced(dev->d3d9_depthbuffer);
    request.display_mode = d3d9_device_display_mode(request.present);
    recomp::d3d::D3d9ResetResult result{};
    d3d9_reset_transaction(dev, request, result);
    wr32(present + 0, result.present.width);
    wr32(present + 4, result.present.height);
    wr32(present + 8, result.present.format);
    wr32(present + 12, result.present.count);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    gdi_forget_surface(dev->id);
    if (dev->hwnd != device_window)
        win32_uncover_display(c, dev->hwnd);
    dev->hwnd = device_window;
    device_discard_implicit_surfaces(dev);
    d3d9_sync_virtual_display(c, dev, dev->hwnd);
    LOGW("d3d9: Reset to %ux%u windowed=%u", request.present.width, request.present.height,
         request.present.windowed);
    com_ret(c, D3D_OK9);
}

void Dev_TestCooperativeLevel(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->test_cooperative_level();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (result.value == 1) {
        com_ret(c, D3DERR_DEVICELOST);
        return;
    }
    if (result.value == 2) {
        com_ret(c, D3DERR_DEVICENOTRESET);
        return;
    }
    com_ret(c, D3D_OK9);
}
void Dev_GetAvailableTextureMem(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->available_texture_mem();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        std::string message(result.status.message.data(), result.status.message.size());
        LOGW("d3d9: GetAvailableTextureMem: %s", message.c_str());
        imports_unsupported(c);
        return;
    }
    set_eax(c, result.value);
}
void Dev_GetNumberOfSwapChains(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    set_eax(c, dev->d3d9_device->number_of_swap_chains().value);
}
// (this, iSwapChain, pMode)
void Dev_GetDisplayMode(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !out || !gm_valid(out, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (arg(c, 1) != 0 || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9DisplayMode mode = d3d9_current_display_mode();
    recomp::d3d::D3d9DisplayModeRequest request{};
    request.swap_chain = arg(c, 1);
    request.width = mode.width;
    request.height = mode.height;
    request.refresh_rate = mode.refresh_rate;
    request.format = mode.format;
    recomp::d3d::D3d9DisplayModeResult result = dev->d3d9_device->display_mode(request);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    wr32(out + 0, result.width);
    wr32(out + 4, result.height);
    wr32(out + 8, result.refresh_rate);
    wr32(out + 12, result.format);
    com_ret(c, D3D_OK9);
}
void Dev_GetDirect3D(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *d3d = dev ? com_get(dev->dev_d3d) : nullptr;
    uint32_t out = arg(c, 1);
    if (!d3d || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(d3d, IF_D3D9);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(d3d);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}
void Dev_GetCreationParameters(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CreationParametersResult result =
        recomp::d3d::d3d9_creation_parameters(*dev->d3d9_device->value);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    wr32(out + 0, result.parameters.adapter);
    wr32(out + 4, result.parameters.device_type);
    wr32(out + 8, dev->d3d9_focus_window);
    wr32(out + 12, result.parameters.behavior_flags);
    com_ret(c, D3D_OK9);
}
void Dev_GetDeviceCaps(X86 *c) {
    uint32_t out = arg(c, 1);
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 304)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9CapsResult result = dev->d3d9_device->caps();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (result.caps.size() >= 304)
        memcpy(gm_ptr(out), result.caps.data(), 304);
    com_ret(c, D3D_OK9);
}
void Dev_EvictManagedResources(X86 *c) {
    com_ret(c, this_device9(c) ? D3D_OK9 : D3DERR_INVALIDCALL);
}
void Dev_BeginScene(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->begin_scene()));
}
void Dev_EndScene(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->end_scene()));
}
// Defined with the resources below.
static ComObj *device_backbuffer(ComObj *dev);
static uint8_t *resource_bytes(ComObj *o);

// (this, Count, pRects, Flags, Color, Z, Stencil). Only the colour target is
// real so far: a 32-bit surface is filled with the colour, over the listed
// D3DRECTs or the whole surface.
void Dev_Clear(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t count = arg(c, 1), rects = arg(c, 2), flags = arg(c, 3), color = arg(c, 4);
    if (!dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if ((!count && rects) ||
        (count && (count > 0x0fffffffu || !rects || !gm_fits(rects, count * 16u)))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<recomp::d3d::D3d9Rect> list;
    list.reserve(count);
    for (uint32_t i = 0; i < count; ++i) {
        recomp::d3d::D3d9Rect r;
        uint32_t a = rects + i * 16u;
        r.left = (int32_t)rd32(a);
        r.top = (int32_t)rd32(a + 4);
        r.right = (int32_t)rd32(a + 8);
        r.bottom = (int32_t)rd32(a + 12);
        list.push_back(r);
    }
    float z;
    uint32_t zbits = arg(c, 5);
    memcpy(&z, &zbits, 4);
    rust::Slice<const recomp::d3d::D3d9Rect> rect_slice(list.empty() ? nullptr : list.data(),
                                                        list.size());
    com_ret(c, bridge_result(c, dev->d3d9_device->clear(flags, color, z, arg(c, 6), rect_slice)));
}

// (this, pSourceRect, pDestRect, hDestWindowOverride, pDirtyRegion). The back
// buffer goes to the host as a 32-bit frame: the first thing this module puts
// on screen.
void Dev_Present(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (arg(c, 3) && arg(c, 3) != dev->hwnd) {
        LOGW("d3d9: Present hDestWindowOverride %08x is not the device window %08x", arg(c, 3),
             dev->hwnd);
        imports_unsupported(c);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9Rect sr{}, dr{};
    bool have_src = arg(c, 1) != 0;
    bool have_dst = arg(c, 2) != 0;
    if ((have_src && !read_guest_rect(arg(c, 1), &sr)) ||
        (have_dst && !read_guest_rect(arg(c, 2), &dr))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const recomp::d3d::D3d9Rect> srs(have_src ? &sr : nullptr, have_src ? 1 : 0);
    rust::Slice<const recomp::d3d::D3d9Rect> drs(have_dst ? &dr : nullptr, have_dst ? 1 : 0);
    const auto present_params = dev->d3d9_device->effective_present();
    const bool fullscreen = present_params.windowed == 0;
    int32_t client_w = 0, client_h = 0;
    const bool direct =
        fullscreen ||
        (host_window_visible(dev->hwnd) &&
         host_window_rect(dev->hwnd, nullptr, nullptr, &client_w, &client_h) &&
         client_w == int32_t(present_params.width) && client_h == int32_t(present_params.height));
    bool presented = false;
    if (!dx_dump_enabled() && direct && dev->d3d9_device->native_present_available) {
        auto result = dev->d3d9_device->present_handoff(srs, drs);
        if (result.status.code == recomp::d3d::StatusCode::Ok) {
            presented = host_display_present_owned_texture(
                result.handoff.texture, (int)result.handoff.width, (int)result.handoff.height,
                result.handoff.ready_event, result.handoff.ready_value,
                reinterpret_cast<void *>(result.handoff.lease), [](void *lease) {
                    recomp::d3d::release_present_lease(reinterpret_cast<size_t>(lease));
                });
            if (presented) {
                dev->d3d9_device->commit_present();
                gdi_present_external(dev->id, dev->hwnd, (int)result.handoff.width,
                                     (int)result.handoff.height, fullscreen, !fullscreen,
                                     fetch_presented);
            } else {
                dev->d3d9_device->native_present_available = false;
                LOGW("d3d9: presenter declined native texture handoff; using RGBA readback");
            }
        } else if (result.status.code != recomp::d3d::StatusCode::Unsupported) {
            com_ret(c, bridge_result(c, result.status));
            return;
        } else {
            dev->d3d9_device->native_present_available = false;
            std::string message(result.status.message.data(), result.status.message.size());
            LOGW("d3d9: %s; using RGBA readback", message.c_str());
        }
    }
    if (!presented) {
        recomp::d3d::D3d9FrameResult result = dev->d3d9_device->present(srs, drs);
        if (result.status.code != recomp::d3d::StatusCode::Ok) {
            com_ret(c, bridge_result(c, result.status));
            return;
        }
        if (!result.bytes.empty() && result.width && result.height) {
            dx_dump_frame_rgba(result.bytes.data(), result.width, result.height);
            const size_t count = size_t(result.width) * result.height;
            std::vector<uint32_t> argb(count);
            for (size_t i = 0; i < count; ++i)
                argb[i] = 0xff000000u | uint32_t(result.bytes[i * 4]) << 16 |
                          uint32_t(result.bytes[i * 4 + 1]) << 8 |
                          uint32_t(result.bytes[i * 4 + 2]);
            gdi_present_surface(dev->id, dev->hwnd, argb.data(), (int)result.width,
                                (int)result.height, fullscreen, !fullscreen);
        }
    }
    const uint32_t interval = dev->d3d9_device->effective_present().presentation_interval;
    if (interval != 0x80000000u) {
        const int refreshes = interval >= 8u ? 4 : interval >= 4u ? 3 : interval == 2u ? 2 : 1;
        const double delay = host_present_refresh_delay(refreshes);
        if (delay > 0)
            guest_sleep_ms((uint32_t)(delay * 1000.0));
    }
    com_ret(c, D3D_OK9);
}

// ---------------------------------------------------------------------------
// Device resources
//
// A game creates textures, buffers and surfaces before it draws anything, and
// then writes its geometry and its images into them. None of that reaches a
// GPU yet, but the storage is real guest memory, so a Lock hands back a
// pointer the game can fill and the contents survive to be drawn later.
// ---------------------------------------------------------------------------
// A resource's contents live in host memory, as a driver's would: the guest
// heap is the game's, and a texture set would exhaust it. The guest sees the
// bytes only while the resource is locked, through a copy made on the first
// lock and written back on the last unlock.
static uint32_t resource_alloc(ComObj *o, uint32_t bytes) {
    if (!bytes)
        return 0;
    o->blob.assign(bytes, 0);
    o->pixels = 0;
    o->pixels_bytes = bytes;
    return 1;
}

// The bytes to read or write right now: the guest copy while locked.
static uint8_t *resource_bytes(ComObj *o) {
    if (!o)
        return nullptr;
    if (o->pixels)
        return gm_ptr(o->pixels);
    return o->blob.empty() ? nullptr : o->blob.data();
}

static constexpr uint64_t D3D9_RETAINED_STAGING_LIMIT = 64ull * 1024ull * 1024ull;
static uint64_t g_d3d9_retained_staging_bytes = 0;

static void d3d9_free_staging(D3d9Staging &staging) {
    if (staging.addr)
        heap_free(staging.addr);
    if (staging.retain)
        g_d3d9_retained_staging_bytes -= staging.size;
    staging = {};
}

static void d3d9_release_staging(D3d9HostResource *h) {
    if (!h)
        return;
    for (auto &entry : h->staging)
        d3d9_free_staging(entry.second);
    h->staging.clear();
}

static void d3d9_resource_destroy(ComObj *o) {
    if (o->d3d9_resource && o->dev_d3d) {
        if (ComObj *dev = com_get(o->dev_d3d)) {
            if (dev->d3d9_device)
                dev->d3d9_device->unregister_resource(o->id);
        }
    }
    if (!o->d3d9_parent && o->d3d9_resource)
        d3d9_release_staging(o->d3d9_resource.get());
    o->d3d9_resource.reset();
    if (o->d3d9_parent_retained) {
        o->d3d9_parent_retained = false;
        if (ComObj *parent = com_get(o->d3d9_parent))
            com_release_internal(parent);
    }
    o->d3d9_parent = 0;
    if (o->d3d9_owner_retained) {
        o->d3d9_owner_retained = false;
        if (ComObj *dev = com_get(o->dev_d3d))
            com_release_internal(dev);
    }
    o->dev_d3d = 0;
}

static const uint32_t USAGE_RENDERTARGET = 1, USAGE_DEPTHSTENCIL = 2;

static void d3d9_register_wgpu_resource(ComObj *dev, ComObj *o) {
    if (!dev || !dev->d3d9_device || !o || !o->d3d9_resource)
        return;
    recomp::d3d::Status status = dev->d3d9_device->register_resource(o->id, *o->d3d9_resource);
    if (status.code != recomp::d3d::StatusCode::Ok) {
        std::string message(status.message.data(), status.message.size());
        LOGW("d3d9: register resource %u failed: %s", o->id, message.c_str());
    }
}

static void create_buffer_wgpu(X86 *c, recomp::d3d::D3d9BufferKind kind) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 5);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    bool vertex = kind == recomp::d3d::D3d9BufferKind::Vertex;
    recomp::d3d::D3d9BufferRequest request{};
    request.kind = kind;
    request.size = arg(c, 1);
    request.usage = arg(c, 2);
    request.pool = arg(c, 4);
    request.fvf = vertex ? arg(c, 3) : 0;
    request.format = vertex ? 100 : arg(c, 3);
    request.shared_handle = arg(c, 6) != 0;
    auto resource = std::make_shared<D3d9HostResource>();
    recomp::d3d::Status status = resource->create_buffer(*dev->d3d9_device, request);
    if (status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, status));
        return;
    }
    ComKind resource_kind = vertex ? K_D3D9VB : K_D3D9IB;
    ComIface resource_iface = vertex ? IF_D3DVERTEXBUFFER9 : IF_D3DINDEXBUFFER9;
    ComObj *buffer = com_new(resource_kind);
    if (!buffer) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    buffer->d3d9_resource = std::move(resource);
    buffer->dev_d3d = dev->id;
    com_retain_internal(dev);
    buffer->d3d9_owner_retained = true;
    d3d9_register_wgpu_resource(dev, buffer);
    uint32_t view = com_view(buffer, resource_iface);
    if (!view) {
        com_release(buffer);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

static ComObj *level_owner(ComObj *o, uint32_t *face, uint32_t *level, bool *volume) {
    *face = 0;
    *level = 0;
    *volume = false;
    if (!o)
        return nullptr;
    if (o->d3d9_parent) {
        *face = o->d3d9_face;
        *level = o->d3d9_level;
        *volume = o->d3d9_volume;
        return com_get(o->d3d9_parent);
    }
    return o;
}

static ComObj *com_level_view(ComObj *tex, uint32_t face, uint32_t level, bool volume) {
    if (!tex || !tex->d3d9_resource)
        return nullptr;
    uint32_t slot;
    if (volume)
        slot = level;
    else if (tex->kind == K_D3D9CUBETEXTURE)
        slot = face + 6u * level;
    else
        slot = level;
    if (tex->d3d9_levels.size() <= slot)
        tex->d3d9_levels.resize(slot + 1, 0);
    ComObj *s = com_get(tex->d3d9_levels[slot]);
    if (s) {
        com_addref(s);
        return s;
    }
    s = com_new(volume ? K_D3D9VOLUME : K_D3D9SURFACE);
    if (!s)
        return nullptr;
    s->d3d9_resource = tex->d3d9_resource;
    s->d3d9_parent = tex->id;
    s->d3d9_face = face;
    s->d3d9_level = level;
    s->d3d9_volume = volume;
    s->dev_d3d = tex->dev_d3d;
    if (ComObj *dev = com_get(tex->dev_d3d)) {
        com_retain_internal(dev);
        s->d3d9_owner_retained = true;
    }
    com_retain_internal(tex);
    s->d3d9_parent_retained = true;
    tex->d3d9_levels[slot] = s->id;
    if (s->dev_d3d) {
        if (ComObj *dev = com_get(s->dev_d3d)) {
            if (dev->d3d9_device) {
                recomp::d3d::Status status = dev->d3d9_device->register_resource_view(
                    s->id, *s->d3d9_resource, face, level, volume);
                if (!d3d_bridge_ok(status)) {
                    tex->d3d9_levels[slot] = 0;
                    com_release(s);
                    return nullptr;
                }
            }
        }
    }
    return s;
}

static void create_texture_wgpu(X86 *c, recomp::d3d::D3d9TextureKind kind) {
    ComObj *dev = this_device9(c);
    recomp::d3d::D3d9TextureRequest request{};
    request.kind = kind;
    uint32_t out = 0;
    ComIface iface = IF_D3DTEXTURE9;
    ComKind com_kind = K_D3D9TEXTURE;
    switch (kind) {
    case recomp::d3d::D3d9TextureKind::Texture2D:
        request.width = arg(c, 1);
        request.height = arg(c, 2);
        request.levels = arg(c, 3);
        request.usage = arg(c, 4);
        request.format = arg(c, 5);
        request.pool = arg(c, 6);
        out = arg(c, 7);
        request.shared_handle = arg(c, 8) != 0;
        break;
    case recomp::d3d::D3d9TextureKind::Cube:
        request.width = request.height = arg(c, 1);
        request.levels = arg(c, 2);
        request.usage = arg(c, 3);
        request.format = arg(c, 4);
        request.pool = arg(c, 5);
        out = arg(c, 6);
        request.shared_handle = arg(c, 7) != 0;
        iface = IF_D3DCUBETEXTURE9;
        com_kind = K_D3D9CUBETEXTURE;
        break;
    case recomp::d3d::D3d9TextureKind::Volume:
        request.width = arg(c, 1);
        request.height = arg(c, 2);
        request.depth = arg(c, 3);
        request.levels = arg(c, 4);
        request.usage = arg(c, 5);
        request.format = arg(c, 6);
        request.pool = arg(c, 7);
        out = arg(c, 8);
        request.shared_handle = arg(c, 9) != 0;
        iface = IF_D3DVOLUMETEXTURE9;
        com_kind = K_D3D9VOLUMETEXTURE;
        break;
    }
    request.depth = request.depth ? request.depth : 1;
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    auto resource = std::make_shared<D3d9HostResource>();
    recomp::d3d::D3d9TextureCreateResult result =
        resource->create_texture(*dev->d3d9_device, request);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *tex = com_new(com_kind);
    if (!tex) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    tex->d3d9_resource = std::move(resource);
    tex->dev_d3d = dev->id;
    com_retain_internal(dev);
    tex->d3d9_owner_retained = true;
    d3d9_register_wgpu_resource(dev, tex);
    uint32_t view = com_view(tex, iface);
    if (!view) {
        com_release(tex);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

static void create_surface_wgpu(X86 *c, uint32_t out_index, uint32_t usage) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, out_index);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9SurfaceRequest request{};
    request.width = arg(c, 1);
    request.height = arg(c, 2);
    request.format = arg(c, 3);
    if (out_index == 5) {
        request.pool = arg(c, 4);
        request.usage = 0;
        request.shared_handle = arg(c, 6) != 0;
    } else {
        request.pool = 0;
        request.usage = usage;
        request.multisample_type = arg(c, 4);
        request.multisample_quality = arg(c, 5);
        request.lockable = usage == USAGE_RENDERTARGET && arg(c, 6) != 0;
        request.discard = usage == USAGE_DEPTHSTENCIL && arg(c, 6) != 0;
        request.shared_handle = arg(c, 8) != 0;
    }
    auto resource = std::make_shared<D3d9HostResource>();
    recomp::d3d::Status status = resource->create_surface(*dev->d3d9_device, request);
    if (status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, status));
        return;
    }
    ComObj *s = com_new(K_D3D9SURFACE);
    if (!s) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    s->d3d9_resource = std::move(resource);
    s->dev_d3d = dev->id;
    com_retain_internal(dev);
    s->d3d9_owner_retained = true;
    d3d9_register_wgpu_resource(dev, s);
    uint32_t view = com_view(s, IF_D3DSURFACE9);
    if (!view) {
        com_release(s);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

static bool d3dx_srcinfo_ok(uint32_t addr) {
    return addr == 0 || gm_valid(addr, 28);
}

static bool d3dx_palette_ok(uint32_t addr) {
    return addr == 0 || gm_valid(addr, 1024);
}

static bool d3dx_read_file(const std::string &path, std::vector<uint8_t> &bytes) {
    if (path.empty())
        return false;
    int fd = os_fd_open(path.c_str(), OS_O_RDONLY);
    if (fd < 0)
        return false;
    OsStat st{};
    if (os_fd_stat(fd, &st) != 0 || !st.is_regular || !st.size || st.size > INT32_MAX) {
        os_fd_close(fd);
        return false;
    }
    bytes.resize((size_t)st.size);
    size_t got = 0;
    while (got < bytes.size()) {
        int64_t n = os_fd_read(fd, bytes.data() + got, bytes.size() - got);
        if (n <= 0)
            break;
        got += (size_t)n;
    }
    os_fd_close(fd);
    return got == bytes.size();
}

static bool d3dx_write_file(const std::string &path, const uint8_t *data, size_t size) {
    if (path.empty())
        return false;
    int fd = os_fd_open(path.c_str(), OS_O_WRONLY | OS_O_CREAT | OS_O_TRUNC);
    if (fd < 0)
        return false;
    size_t put = 0;
    while (put < size) {
        int64_t n = os_fd_write(fd, data + put, size - put);
        if (n <= 0)
            break;
        put += (size_t)n;
    }
    os_fd_close(fd);
    return put == size;
}

static void d3dx_write_info(uint32_t addr, const recomp::d3d::D3dxImageInfo &info) {
    if (!addr || !gm_valid(addr, 28))
        return;
    wr32(addr + 0, info.width);
    wr32(addr + 4, info.height);
    wr32(addr + 8, info.depth);
    wr32(addr + 12, info.mip_levels);
    wr32(addr + 16, info.format);
    wr32(addr + 20, info.resource_type);
    wr32(addr + 24, info.image_file_format);
}

static ComKind d3dx_com_kind(recomp::d3d::D3d9TextureKind kind) {
    switch (kind) {
    case recomp::d3d::D3d9TextureKind::Cube:
        return K_D3D9CUBETEXTURE;
    case recomp::d3d::D3d9TextureKind::Volume:
        return K_D3D9VOLUMETEXTURE;
    default:
        return K_D3D9TEXTURE;
    }
}

static ComIface d3dx_iface(recomp::d3d::D3d9TextureKind kind) {
    switch (kind) {
    case recomp::d3d::D3d9TextureKind::Cube:
        return IF_D3DCUBETEXTURE9;
    case recomp::d3d::D3d9TextureKind::Volume:
        return IF_D3DVOLUMETEXTURE9;
    default:
        return IF_D3DTEXTURE9;
    }
}

static void d3dx_finish_create(X86 *c, ComObj *dev,
                               const std::shared_ptr<D3d9HostResource> &resource,
                               const recomp::d3d::D3dxCreateResult &result,
                               recomp::d3d::D3d9TextureKind kind, uint32_t out_addr,
                               uint32_t srcinfo_addr) {
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    d3dx_write_info(srcinfo_addr, result.info);
    ComObj *tex = com_new(d3dx_com_kind(kind));
    if (!tex) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    tex->d3d9_resource = resource;
    tex->dev_d3d = dev->id;
    com_retain_internal(dev);
    tex->d3d9_owner_retained = true;
    d3d9_register_wgpu_resource(dev, tex);
    uint32_t view = com_view(tex, d3dx_iface(kind));
    if (!view) {
        com_release(tex);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out_addr, view);
    com_ret(c, D3D_OK9);
}

static void d3dx_create_from_file_ex(X86 *c, recomp::d3d::D3d9TextureKind kind) {
    ComObj *dev = this_device9(c);
    uint32_t file_addr = arg(c, 1);
    recomp::d3d::D3dxCreateRequest request{};
    request.kind = kind;
    uint32_t out_addr = 0, srcinfo_addr = 0, palette_addr = 0;
    switch (kind) {
    case recomp::d3d::D3d9TextureKind::Texture2D:
        request.width = arg(c, 2);
        request.height = arg(c, 3);
        request.levels = arg(c, 4);
        request.usage = arg(c, 5);
        request.format = arg(c, 6);
        request.pool = arg(c, 7);
        request.filter = arg(c, 8);
        request.mip_filter = arg(c, 9);
        request.color_key = arg(c, 10);
        srcinfo_addr = arg(c, 11);
        palette_addr = arg(c, 12);
        out_addr = arg(c, 13);
        request.depth = 1;
        break;
    case recomp::d3d::D3d9TextureKind::Cube:
        request.width = request.height = arg(c, 2);
        request.levels = arg(c, 3);
        request.usage = arg(c, 4);
        request.format = arg(c, 5);
        request.pool = arg(c, 6);
        request.filter = arg(c, 7);
        request.mip_filter = arg(c, 8);
        request.color_key = arg(c, 9);
        srcinfo_addr = arg(c, 10);
        palette_addr = arg(c, 11);
        out_addr = arg(c, 12);
        request.depth = 1;
        break;
    case recomp::d3d::D3d9TextureKind::Volume:
        request.width = arg(c, 2);
        request.height = arg(c, 3);
        request.depth = arg(c, 4);
        request.levels = arg(c, 5);
        request.usage = arg(c, 6);
        request.format = arg(c, 7);
        request.pool = arg(c, 8);
        request.filter = arg(c, 9);
        request.mip_filter = arg(c, 10);
        request.color_key = arg(c, 11);
        srcinfo_addr = arg(c, 12);
        palette_addr = arg(c, 13);
        out_addr = arg(c, 14);
        break;
    default:
        break;
    }
    if (!dev || !dev->d3d9_device || !out_addr || !gm_valid(out_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!d3dx_srcinfo_ok(srcinfo_addr) || !d3dx_palette_ok(palette_addr)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (palette_addr) {
        imports_unsupported(c);
        return;
    }
    com_out_ptr(out_addr, 0);
    if (!file_addr || !gm_valid(file_addr, 1)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::string path = win32_host_path_op(gm_str(file_addr, 1024), WIN32_FILE_READ);
    std::vector<uint8_t> bytes;
    if (!d3dx_read_file(path, bytes)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    auto resource = std::make_shared<D3d9HostResource>();
    recomp::d3d::D3dxCreateResult result = resource->create_d3dx_texture(
        *dev->d3d9_device, request, rust::Slice<const uint8_t>(bytes.data(), bytes.size()));
    d3dx_finish_create(c, dev, resource, result, kind, out_addr, srcinfo_addr);
}

static void d3dx_create_from_memory(X86 *c, recomp::d3d::D3d9TextureKind kind) {
    ComObj *dev = this_device9(c);
    uint32_t data_addr = arg(c, 1), size = arg(c, 2), out_addr = arg(c, 3);
    if (!dev || !dev->d3d9_device || !out_addr || !gm_valid(out_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out_addr, 0);
    if (!data_addr || !size || !gm_valid(data_addr, size)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxCreateRequest request{};
    request.kind = kind;
    request.width = 0xFFFFFFFFu;
    request.height = 0xFFFFFFFFu;
    request.depth = 0xFFFFFFFFu;
    request.levels = 0xFFFFFFFFu;
    request.usage = 0;
    request.format = 0;
    request.pool = 1;
    request.filter = 0xFFFFFFFFu;
    request.mip_filter = 0xFFFFFFFFu;
    request.color_key = 0;
    auto resource = std::make_shared<D3d9HostResource>();
    recomp::d3d::D3dxCreateResult result = resource->create_d3dx_texture(
        *dev->d3d9_device, request,
        rust::Slice<const uint8_t>((const uint8_t *)gm_ptr(data_addr), size));
    d3dx_finish_create(c, dev, resource, result, kind, out_addr, 0);
}

void X_D3DXGetImageInfoFromFileInMemory(X86 *c) {
    uint32_t data_addr = arg(c, 0), size = arg(c, 1), info_addr = arg(c, 2);
    if (!data_addr || !size || !gm_valid(data_addr, size) || !d3dx_srcinfo_ok(info_addr)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxImageInfoResult result =
        d3dx_image_info(rust::Slice<const uint8_t>((const uint8_t *)gm_ptr(data_addr), size));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    d3dx_write_info(info_addr, result.info);
    com_ret(c, D3D_OK9);
}

void X_D3DXCreateTextureFromFileExA(X86 *c) {
    d3dx_create_from_file_ex(c, recomp::d3d::D3d9TextureKind::Texture2D);
}
void X_D3DXCreateCubeTextureFromFileExA(X86 *c) {
    d3dx_create_from_file_ex(c, recomp::d3d::D3d9TextureKind::Cube);
}
void X_D3DXCreateVolumeTextureFromFileExA(X86 *c) {
    d3dx_create_from_file_ex(c, recomp::d3d::D3d9TextureKind::Volume);
}
void X_D3DXCreateTextureFromFileInMemory(X86 *c) {
    d3dx_create_from_memory(c, recomp::d3d::D3d9TextureKind::Texture2D);
}
void X_D3DXCreateCubeTextureFromFileInMemory(X86 *c) {
    d3dx_create_from_memory(c, recomp::d3d::D3d9TextureKind::Cube);
}
void X_D3DXCreateVolumeTextureFromFileInMemory(X86 *c) {
    d3dx_create_from_memory(c, recomp::d3d::D3d9TextureKind::Volume);
}

static uint8_t d3dx_fill_byte(float value) {
    if (!(value > 0.0f))
        return 0;
    if (value >= 1.0f)
        return 255;
    return (uint8_t)(value * 255.0f + 0.5f);
}

static uint32_t d3dx_fill_pixel(uint32_t format, const float c[4], uint8_t out[4]) {
    uint8_t r = d3dx_fill_byte(c[0]), g = d3dx_fill_byte(c[1]), b = d3dx_fill_byte(c[2]),
            a = d3dx_fill_byte(c[3]);
    auto put = [out](std::initializer_list<uint8_t> v) {
        std::copy(v.begin(), v.end(), out);
        return (uint32_t)v.size();
    };
    switch (format) {
    case 20:
        return put({b, g, r});
    case 21:
        return put({b, g, r, a});
    case 22:
        return put({b, g, r, 255});
    case 28:
        return put({a});
    case 32:
        return put({r, g, b, a});
    case 33:
        return put({r, g, b, 255});
    case 50:
        return put({r});
    case 51:
        return put({r, a});
    default:
        return 0;
    }
}

void X_D3DXFillVolumeTexture(X86 *c) {
    ComObj *tex = com_this(arg(c, 0), IF_D3DVOLUMETEXTURE9);
    uint32_t callback = arg(c, 1), user = arg(c, 2);
    if (!tex || !tex->d3d9_resource || !callback) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult td = tex->d3d9_resource->texture_descriptor();
    if (td.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, td.status));
        return;
    }
    const float zero[4] = {};
    uint8_t pixel[4];
    uint32_t bytes = d3dx_fill_pixel(td.descriptor.format, zero, pixel);
    if (!bytes) {
        LOGW("d3dx9: D3DXFillVolumeTexture: unsupported format %u", td.descriptor.format);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t scratch = heap_alloc(64, true, 16);
    if (!scratch) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    uint32_t out = scratch, coord = scratch + 16, texel = scratch + 28, lock = scratch + 40;
    uint32_t result = D3D_OK9;
    for (uint32_t level = 0; level < td.descriptor.levels; ++level) {
        recomp::d3d::D3d9LevelDescriptorResult ld =
            tex->d3d9_resource->texture_volume_descriptor(level);
        if (ld.status.code != recomp::d3d::StatusCode::Ok) {
            result = bridge_result(c, ld.status);
            break;
        }
        level_lock_wgpu(c, tex, 0, level, true, 0, 0, 0, lock, true);
        result = c->r[0];
        if (result != D3D_OK9)
            break;
        uint32_t pitch = rd32(lock), slice_pitch = rd32(lock + 4), bits = rd32(lock + 8);
        uint32_t w = ld.descriptor.width, h = ld.descriptor.height, d = ld.descriptor.depth;
        for (uint32_t z = 0; z < d; ++z) {
            for (uint32_t y = 0; y < h; ++y) {
                for (uint32_t x = 0; x < w; ++x) {
                    memset(gm_ptr(out), 0, 16);
                    d3dx_wf(coord, ((float)x + 0.5f) / (float)w);
                    d3dx_wf(coord + 4, ((float)y + 0.5f) / (float)h);
                    d3dx_wf(coord + 8, ((float)z + 0.5f) / (float)d);
                    d3dx_wf(texel, 1.0f / (float)w);
                    d3dx_wf(texel + 4, 1.0f / (float)h);
                    d3dx_wf(texel + 8, 1.0f / (float)d);
                    guest_call(c, callback, out, coord, texel, user);
                    float color[4] = {d3dx_rf(out), d3dx_rf(out + 4), d3dx_rf(out + 8),
                                      d3dx_rf(out + 12)};
                    d3dx_fill_pixel(td.descriptor.format, color, pixel);
                    memcpy(gm_ptr(bits + z * slice_pitch + y * pitch + x * bytes), pixel, bytes);
                }
            }
        }
        level_unlock_wgpu(c, tex, 0, level);
        if (c->r[0] != D3D_OK9) {
            result = c->r[0];
            break;
        }
    }
    heap_free(scratch);
    com_ret(c, result);
}

static bool d3dx_rect(uint32_t addr, recomp::d3d::D3d9Rect &rect, bool &present) {
    present = false;
    rect.left = rect.top = rect.right = rect.bottom = 0;
    if (!addr)
        return true;
    if (!gm_valid(addr, 16))
        return false;
    present = true;
    rect.left = (int32_t)rd32(addr + 0);
    rect.top = (int32_t)rd32(addr + 4);
    rect.right = (int32_t)rd32(addr + 8);
    rect.bottom = (int32_t)rd32(addr + 12);
    return true;
}

static ComObj *d3dx_device_of(ComObj *node) {
    if (!node)
        return nullptr;
    ComObj *dev = com_get(node->dev_d3d);
    if (!dev || !dev->d3d9_device)
        return nullptr;
    return dev;
}

void X_D3DXLoadSurfaceFromSurface(X86 *c) {
    ComObj *dst = com_this(arg(c, 0));
    ComObj *src = com_this(arg(c, 3));
    if (!dst || !src || dst->kind != K_D3D9SURFACE || src->kind != K_D3D9SURFACE) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!d3dx_palette_ok(arg(c, 1)) || !d3dx_palette_ok(arg(c, 4))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9Rect drect{}, srect{};
    bool dpresent = false, spresent = false;
    if (!d3dx_rect(arg(c, 2), drect, dpresent) || !d3dx_rect(arg(c, 5), srect, spresent)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t df = 0, dl = 0, sf = 0, sl = 0;
    bool dv = false, sv = false;
    ComObj *downer = level_owner(dst, &df, &dl, &dv);
    ComObj *sowner = level_owner(src, &sf, &sl, &sv);
    ComObj *dev = d3dx_device_of(downer);
    if (!downer || !sowner || !downer->d3d9_resource || !sowner->d3d9_resource || !dev) {
        imports_unsupported(c);
        return;
    }
    if (downer->dev_d3d != sowner->dev_d3d || dv || sv) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t filter = arg(c, 6), color_key = arg(c, 7);
    recomp::d3d::D3dxSurfaceReadResult read = dev->d3d9_device->d3dx_surface_read(
        *sowner->d3d9_resource, sf, sl, srect, spresent, color_key);
    if (read.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, read.status));
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->d3dx_surface_store(
        *downer->d3d9_resource, df, dl, drect, dpresent, *read.snapshot, filter);
    com_ret(c, bridge_result(c, status));
}

static bool d3dx_encode_surface(X86 *c, recomp::d3d::D3dxBytesResult &result) {
    uint32_t format = arg(c, 1);
    ComObj *src = com_this(arg(c, 2));
    if (!src || src->kind != K_D3D9SURFACE) {
        com_ret(c, D3DERR_INVALIDCALL);
        return false;
    }
    if (!d3dx_palette_ok(arg(c, 3))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return false;
    }
    recomp::d3d::D3d9Rect rect{};
    bool present = false;
    if (!d3dx_rect(arg(c, 4), rect, present)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return false;
    }
    uint32_t face = 0, level = 0;
    bool volume = false;
    ComObj *owner = level_owner(src, &face, &level, &volume);
    ComObj *dev = d3dx_device_of(owner);
    if (!owner || !owner->d3d9_resource || !dev || volume) {
        com_ret(c, D3DERR_INVALIDCALL);
        return false;
    }
    result = dev->d3d9_device->d3dx_save_surface(*owner->d3d9_resource, face, level, rect, present,
                                                 format);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return false;
    }
    return true;
}

void X_D3DXSaveSurfaceToFileA(X86 *c) {
    uint32_t file_addr = arg(c, 0);
    if (!file_addr || !gm_valid(file_addr, 1)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxBytesResult result;
    if (!d3dx_encode_surface(c, result))
        return;
    std::string path = win32_host_path_op(gm_str(file_addr, 1024), WIN32_FILE_WRITE);
    if (!d3dx_write_file(path, result.bytes.data(), result.bytes.size())) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, D3D_OK9);
}

void X_D3DXSaveSurfaceToFileInMemory(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    recomp::d3d::D3dxBytesResult result;
    if (!d3dx_encode_surface(c, result))
        return;
    uint32_t view = d3dx_make_buffer(result.bytes.data(), (uint32_t)result.bytes.size());
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(out, view);
    com_ret(c, D3D_OK9);
}

void X_D3DXSaveTextureToFileA(X86 *c) {
    uint32_t file_addr = arg(c, 0), format = arg(c, 1);
    ComObj *src = com_this(arg(c, 2));
    if (!src || (src->kind != K_D3D9TEXTURE && src->kind != K_D3D9CUBETEXTURE &&
                 src->kind != K_D3D9VOLUMETEXTURE)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!d3dx_palette_ok(arg(c, 3))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t face = 0, level = 0;
    bool volume = false;
    ComObj *owner = level_owner(src, &face, &level, &volume);
    ComObj *dev = d3dx_device_of(owner);
    if (!owner || !owner->d3d9_resource || !dev || !file_addr || !gm_valid(file_addr, 1)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxBytesResult result =
        dev->d3d9_device->d3dx_save_texture(*owner->d3d9_resource, format);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    std::string path = win32_host_path_op(gm_str(file_addr, 1024), WIN32_FILE_WRITE);
    if (!d3dx_write_file(path, result.bytes.data(), result.bytes.size())) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, D3D_OK9);
}

static float d3dx_rf(uint32_t addr) {
    uint32_t bits = rd32(addr);
    float value;
    memcpy(&value, &bits, 4);
    return value;
}
static void d3dx_wf(uint32_t addr, float value) {
    uint32_t bits;
    memcpy(&bits, &value, 4);
    wr32(addr, bits);
}

static void d3d9_stage_shader(uint32_t addr, std::vector<uint32_t> &out);

static bool d3dx_stage_shader(uint32_t addr, std::vector<uint32_t> &out) {
    d3d9_stage_shader(addr, out);
    return !out.empty();
}

static ComObj *d3dx_ctab_this(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || o->kind != K_D3DXCONSTANTTABLE || !o->d3dx_ctab)
        return nullptr;
    return o;
}

static int d3dx_ctab_node_of(ComObj *o, uint32_t handle) {
    if (!o->d3dx_ctab_handles || handle < o->d3dx_ctab_handles)
        return -1;
    uint32_t offset = handle - o->d3dx_ctab_handles;
    if (offset % 4 || offset / 4 >= o->d3dx_ctab_nodes)
        return -1;
    return (int)(offset / 4);
}

static int d3dx_ctab_resolve(ComObj *o, uint32_t handle) {
    if (!handle)
        return -1;
    int node = d3dx_ctab_node_of(o, handle);
    if (node >= 0)
        return node;
    if (!gm_valid(handle, 1))
        return -1;
    std::string name = gm_str(handle, 16384);
    if (name.empty() || !gm_valid(handle, name.size() + 1) || rd8(handle + name.size()) != 0)
        return -1;
    return recomp::d3d::d3dx_constant_table_find(
        *o->d3dx_ctab->value, 0xffffffffu,
        rust::Slice<const uint8_t>((const uint8_t *)name.data(), name.size()));
}

static uint32_t d3dx_ctab_handle(ComObj *o, int node) {
    if (node < 0 || (uint32_t)node >= o->d3dx_ctab_nodes || !o->d3dx_ctab_handles)
        return 0;
    return o->d3dx_ctab_handles + (uint32_t)node * 4;
}

static bool d3dx_ctab_desc(ComObj *o, int node, recomp::d3d::D3dxConstantDesc &desc) {
    if (node < 0)
        return false;
    recomp::d3d::D3dxConstantDescResult result =
        recomp::d3d::d3dx_constant_table_desc(*o->d3dx_ctab->value, (uint32_t)node);
    if (result.status.code != recomp::d3d::StatusCode::Ok)
        return false;
    desc = result.desc;
    return true;
}

static void d3dx_ctab_apply_to(X86 *c, uint32_t device_addr, uint32_t handle, uint32_t setter,
                               const uint8_t *data, size_t size) {
    ComObj *o = d3dx_ctab_this(c);
    int node = o ? d3dx_ctab_resolve(o, handle) : -1;
    if (node < 0) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_this(device_addr);
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = recomp::d3d::d3dx_constant_table_apply(
        *dev->d3d9_device->value, *o->d3dx_ctab->value, (uint32_t)node, setter,
        rust::Slice<const uint8_t>(data, size));
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

static bool d3dx_ctab_input_size(X86 *c, uint32_t handle, uint32_t setter, uint64_t requested,
                                 size_t &size) {
    ComObj *o = d3dx_ctab_this(c);
    int node = o ? d3dx_ctab_resolve(o, handle) : -1;
    if (node < 0) {
        com_ret(c, D3DERR_INVALIDCALL);
        return false;
    }
    size = (size_t)recomp::d3d::d3dx_constant_table_input_size(*o->d3dx_ctab->value, (uint32_t)node,
                                                               setter, requested);
    return true;
}

static void d3dx_ctab_apply_guest(X86 *c, uint32_t device_addr, uint32_t handle, uint32_t setter,
                                  uint32_t data_addr, uint32_t count, uint32_t element) {
    size_t size = 0;
    if (!d3dx_ctab_input_size(c, handle, setter, (uint64_t)count * element, size))
        return;
    if (!data_addr || !gm_fits(data_addr, size)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    const uint8_t *data = (const uint8_t *)gm_ptr(data_addr);
    d3dx_ctab_apply_to(c, device_addr, handle, setter, data, (size_t)size);
}

static void d3dx_ctab_pointer_array(X86 *c, uint32_t setter, uint32_t element) {
    uint32_t device_addr = arg(c, 1), handle = arg(c, 2), values_addr = arg(c, 3),
             count = arg(c, 4);
    size_t size = 0;
    if (!d3dx_ctab_input_size(c, handle, setter, (uint64_t)count * element, size))
        return;
    count = (uint32_t)(size / element);
    std::vector<uint8_t> gathered;
    if (!values_addr || !gm_fits_n(values_addr, count, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (count) {
        gathered.reserve((size_t)count * element);
        for (uint32_t i = 0; i < count; ++i) {
            uint32_t pointer = rd32(values_addr + i * 4);
            if (!pointer || !gm_valid(pointer, element)) {
                com_ret(c, D3DERR_INVALIDCALL);
                return;
            }
            const uint8_t *bytes = (const uint8_t *)gm_ptr(pointer);
            gathered.insert(gathered.end(), bytes, bytes + element);
        }
    }
    d3dx_ctab_apply_to(c, device_addr, handle, setter, gathered.data(), gathered.size());
}

void D3DXBuffer_GetBufferPointer(X86 *c) {
    ComObj *o = com_this(arg(c, 0));
    set_eax(c, o && o->kind == K_D3DXCONSTANTTABLE ? o->d3dx_ctab_buffer : 0);
}

void D3DXBuffer_GetBufferSize(X86 *c) {
    ComObj *o = com_this(arg(c, 0));
    set_eax(c, o && o->kind == K_D3DXCONSTANTTABLE ? o->d3dx_ctab_size : 0);
}

void D3DXConstantTable_GetDesc(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    uint32_t desc = arg(c, 1);
    if (!o || !desc || !gm_valid(desc, 12)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxConstantTableDesc info =
        recomp::d3d::d3dx_constant_table_info(*o->d3dx_ctab->value);
    wr32(desc + 0, o->d3dx_ctab_buffer + info.creator_offset);
    wr32(desc + 4, info.version);
    wr32(desc + 8, info.constants);
    com_ret(c, D3D_OK9);
}

void D3DXConstantTable_GetConstantDesc(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    uint32_t handle = arg(c, 1), desc_addr = arg(c, 2), count_addr = arg(c, 3);
    int node = o ? d3dx_ctab_resolve(o, handle) : -1;
    recomp::d3d::D3dxConstantDesc desc;
    if (node < 0 || !d3dx_ctab_desc(o, node, desc)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if ((count_addr && !gm_valid(count_addr, 4)) || (desc_addr && !gm_valid(desc_addr, 48))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (desc_addr) {
        wr32(desc_addr + 0, o->d3dx_ctab_buffer + desc.name_offset);
        wr32(desc_addr + 4, desc.register_set);
        wr32(desc_addr + 8, desc.register_index);
        wr32(desc_addr + 12, desc.register_count);
        wr32(desc_addr + 16, desc.class_);
        wr32(desc_addr + 20, desc.type_);
        wr32(desc_addr + 24, desc.rows);
        wr32(desc_addr + 28, desc.columns);
        wr32(desc_addr + 32, desc.elements);
        wr32(desc_addr + 36, desc.struct_members);
        wr32(desc_addr + 40, desc.bytes);
        wr32(desc_addr + 44,
             desc.default_offset == 0xffffffffu ? 0 : o->d3dx_ctab_buffer + desc.default_offset);
    }
    if (count_addr)
        wr32(count_addr, 1);
    com_ret(c, D3D_OK9);
}

void D3DXConstantTable_GetSamplerIndex(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    int node = o ? d3dx_ctab_resolve(o, arg(c, 1)) : -1;
    set_eax(c, node < 0 ? 0xffffffffu
                        : recomp::d3d::d3dx_constant_table_sampler(*o->d3dx_ctab->value,
                                                                   (uint32_t)node));
}

void D3DXConstantTable_GetConstant(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    uint32_t handle = arg(c, 1);
    int parent = o && handle ? d3dx_ctab_resolve(o, handle) : -1;
    if (!o || (handle && parent < 0)) {
        set_eax(c, 0);
        return;
    }
    int node = recomp::d3d::d3dx_constant_table_constant(
        *o->d3dx_ctab->value, handle ? (uint32_t)parent : 0xffffffffu, arg(c, 2));
    set_eax(c, d3dx_ctab_handle(o, node));
}

void D3DXConstantTable_GetConstantByName(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    uint32_t handle = arg(c, 1), name_addr = arg(c, 2);
    if (!o || !name_addr || !gm_valid(name_addr, 1)) {
        set_eax(c, 0);
        return;
    }
    int parent = handle ? d3dx_ctab_resolve(o, handle) : -1;
    if (handle && parent < 0) {
        set_eax(c, 0);
        return;
    }
    uint32_t parent_arg = handle ? (uint32_t)parent : 0xffffffffu;
    std::string name = gm_str(name_addr, 16384);
    if (!gm_valid(name_addr, name.size() + 1) || rd8(name_addr + name.size()) != 0) {
        set_eax(c, 0);
        return;
    }
    int node = recomp::d3d::d3dx_constant_table_find(
        *o->d3dx_ctab->value, parent_arg,
        rust::Slice<const uint8_t>((const uint8_t *)name.data(), name.size()));
    set_eax(c, d3dx_ctab_handle(o, node));
}

void D3DXConstantTable_GetConstantElement(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    int node = o ? d3dx_ctab_resolve(o, arg(c, 1)) : -1;
    int element = node < 0 ? -1
                           : recomp::d3d::d3dx_constant_table_element(*o->d3dx_ctab->value,
                                                                      (uint32_t)node, arg(c, 2));
    set_eax(c, o ? d3dx_ctab_handle(o, element) : 0);
}

void D3DXConstantTable_SetDefaults(X86 *c) {
    ComObj *o = d3dx_ctab_this(c);
    ComObj *dev = com_this(arg(c, 1));
    if (!o || !dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status =
        recomp::d3d::d3dx_constant_table_defaults(*dev->d3d9_device->value, *o->d3dx_ctab->value);
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

void D3DXConstantTable_SetValue(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetValue, arg(c, 3), arg(c, 4), 1);
}

void D3DXConstantTable_SetBool(X86 *c) {
    uint32_t value = arg(c, 3);
    d3dx_ctab_apply_to(c, arg(c, 1), arg(c, 2), kD3dxSetBool, (const uint8_t *)&value, 4);
}

void D3DXConstantTable_SetBoolArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetBoolArray, arg(c, 3), arg(c, 4), 4);
}

void D3DXConstantTable_SetInt(X86 *c) {
    uint32_t value = arg(c, 3);
    d3dx_ctab_apply_to(c, arg(c, 1), arg(c, 2), kD3dxSetInt, (const uint8_t *)&value, 4);
}

void D3DXConstantTable_SetIntArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetIntArray, arg(c, 3), arg(c, 4), 4);
}

void D3DXConstantTable_SetFloat(X86 *c) {
    uint32_t value = arg(c, 3);
    d3dx_ctab_apply_to(c, arg(c, 1), arg(c, 2), kD3dxSetFloat, (const uint8_t *)&value, 4);
}

void D3DXConstantTable_SetFloatArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetFloatArray, arg(c, 3), arg(c, 4), 4);
}

void D3DXConstantTable_SetVector(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetVector, arg(c, 3), 1, 16);
}

void D3DXConstantTable_SetVectorArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetVectorArray, arg(c, 3), arg(c, 4), 16);
}

void D3DXConstantTable_SetMatrix(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetMatrix, arg(c, 3), 1, 64);
}

void D3DXConstantTable_SetMatrixArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetMatrixArray, arg(c, 3), arg(c, 4), 64);
}

void D3DXConstantTable_SetMatrixPointerArray(X86 *c) {
    d3dx_ctab_pointer_array(c, kD3dxSetMatrixPointerArray, 64);
}

void D3DXConstantTable_SetMatrixTranspose(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetMatrixTranspose, arg(c, 3), 1, 64);
}

void D3DXConstantTable_SetMatrixTransposeArray(X86 *c) {
    d3dx_ctab_apply_guest(c, arg(c, 1), arg(c, 2), kD3dxSetMatrixTransposeArray, arg(c, 3),
                          arg(c, 4), 64);
}

void D3DXConstantTable_SetMatrixTransposePointerArray(X86 *c) {
    d3dx_ctab_pointer_array(c, kD3dxSetMatrixTransposePointerArray, 64);
}

void d3dx_ctab_destroy(ComObj *o) {
    if (o->d3dx_ctab_buffer) {
        heap_free(o->d3dx_ctab_buffer);
        o->d3dx_ctab_buffer = 0;
    }
    if (o->d3dx_ctab_handles) {
        heap_free(o->d3dx_ctab_handles);
        o->d3dx_ctab_handles = 0;
    }
    o->d3dx_ctab.reset();
}

void X_D3DXGetShaderVersion(X86 *c) {
    uint32_t addr = arg(c, 0);
    uint32_t version = addr && gm_valid(addr, 4) ? rd32(addr) : 0;
    set_eax(c, recomp::d3d::d3dx_shader_version(rust::Slice<const uint32_t>(&version, 1)));
}

void X_D3DXGetShaderInputSemantics(X86 *c) {
    uint32_t code_addr = arg(c, 0), semantics_addr = arg(c, 1), count_addr = arg(c, 2);
    if (!code_addr || !gm_valid(code_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<uint32_t> code;
    if (!d3dx_stage_shader(code_addr, code)) {
        com_ret(c, MAKE_D3DHRESULT(2905));
        return;
    }
    recomp::d3d::D3dxSemanticsResult result = recomp::d3d::d3dx_shader_input_semantics(
        rust::Slice<const uint32_t>(code.data(), code.size()));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t count = (uint32_t)result.semantics.size();
    if ((count_addr && !gm_valid(count_addr, 4)) ||
        (semantics_addr && !gm_fits_n(semantics_addr, count, 8))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (count_addr)
        wr32(count_addr, count);
    if (semantics_addr && count) {
        for (uint32_t i = 0; i < count; ++i) {
            wr32(semantics_addr + i * 8, result.semantics[i].usage);
            wr32(semantics_addr + i * 8 + 4, result.semantics[i].usage_index);
        }
    }
    com_ret(c, D3D_OK9);
}

void X_D3DXGetShaderConstantTable(X86 *c) {
    uint32_t code_addr = arg(c, 0), out_addr = arg(c, 1);
    if (!out_addr || !gm_valid(out_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out_addr, 0);
    if (!code_addr || !gm_valid(code_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<uint32_t> code;
    if (!d3dx_stage_shader(code_addr, code)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    auto host = std::make_shared<D3dxConstantTableHost>();
    recomp::d3d::D3dxConstantTableResult result = recomp::d3d::d3dx_constant_table_create(
        *host->value, rust::Slice<const uint32_t>(code.data(), code.size()));
    if (result.invalid_bytecode) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t buffer = 0;
    if (!result.buffer.empty()) {
        buffer = heap_alloc((uint32_t)result.buffer.size(), true, 16);
        if (!buffer) {
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
        memcpy(gm_ptr(buffer), result.buffer.data(), result.buffer.size());
    }
    uint32_t handles = 0;
    if (result.node_count) {
        handles = heap_alloc(result.node_count * 4, true, 16);
        if (!handles) {
            heap_free(buffer);
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
    }
    ComObj *o = com_new(K_D3DXCONSTANTTABLE);
    if (!o) {
        heap_free(buffer);
        heap_free(handles);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    o->d3dx_ctab = host;
    o->d3dx_ctab_buffer = buffer;
    o->d3dx_ctab_size = (uint32_t)result.buffer.size();
    o->d3dx_ctab_handles = handles;
    o->d3dx_ctab_nodes = result.node_count;
    uint32_t view = com_view(o, IF_D3DXCONSTANTTABLE);
    if (!view) {
        com_release(o);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out_addr, view);
    com_ret(c, D3D_OK9);
}

static std::map<std::string, uint32_t> &d3dx_profile_strings() {
    static std::map<std::string, uint32_t> cache;
    return cache;
}

void d3dx_shader_helpers_reset() {
    d3dx_profile_strings().clear();
}

static ComObj *efx_this(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || o->kind != K_D3DXEFFECT || !o->d3dx_effect)
        return nullptr;
    return o;
}

static D3dxEffectHost *efx_host(ComObj *o) {
    return o && o->d3dx_effect ? o->d3dx_effect.get() : nullptr;
}

static ComObj *efx_device(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx)
        return nullptr;
    ComObj *dev = com_get(fx->d3dx_effect->device_id);
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device)
        return nullptr;
    return dev;
}

static void efx_status(X86 *c, recomp::d3d::Status result) {
    com_ret(c, result.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, result));
}

static uint32_t efx_guest_string(D3dxEffectHost *host, const std::vector<uint8_t> &bytes) {
    std::string key((const char *)bytes.data(), bytes.size());
    auto it = host->strings.find(key);
    if (it != host->strings.end())
        return it->second;
    uint32_t addr = heap_alloc((uint32_t)bytes.size() + 1, true, 16);
    if (!addr)
        return 0;
    if (!bytes.empty())
        memcpy(gm_ptr(addr), bytes.data(), bytes.size());
    ((uint8_t *)gm_ptr(addr))[bytes.size()] = 0;
    host->strings.emplace(std::move(key), addr);
    return addr;
}

static bool efx_texture_kind(ComKind kind) {
    return kind == K_D3D9TEXTURE || kind == K_D3D9CUBETEXTURE || kind == K_D3D9VOLUMETEXTURE;
}

static const uint32_t EFX_KNOWN_CREATION_FLAGS = 0x1fbffu;

static uint32_t efx_wrap_handle(ComObj *fx, uint32_t token) {
    if (!token)
        return 0;
    D3dxEffectHost *host = efx_host(fx);
    auto it = host->handle_tokens.find(token);
    if (it != host->handle_tokens.end())
        return it->second;
    uint32_t addr = heap_alloc(4, false, 16);
    if (!addr)
        return 0;
    wr32(addr, token);
    host->handle_tokens.emplace(token, addr);
    host->handle_reverse.emplace(addr, token);
    return addr;
}

static uint32_t efx_resolve_handle(ComObj *fx, uint32_t guest_handle, uint32_t kind) {
    if (!guest_handle)
        return 0;
    D3dxEffectHost *host = efx_host(fx);
    auto it = host->handle_reverse.find(guest_handle);
    if (it != host->handle_reverse.end())
        return it->second;
    std::string text = gm_str(guest_handle, 16384);
    if (text.empty())
        return 0;
    rust::Slice<const uint8_t> name((const uint8_t *)text.data(), text.size());
    if (kind <= 2) {
        uint32_t token = recomp::d3d::d3dx_effect_find(*host->value, 0, name, kind);
        if (token)
            return token;
    }
    for (uint32_t candidate : {0u, 1u, 2u}) {
        uint32_t token = recomp::d3d::d3dx_effect_find(*host->value, 0, name, candidate);
        if (token)
            return token;
    }
    return 0;
}

static void efx_free_handles(D3dxEffectHost *host) {
    for (auto &entry : host->handle_tokens)
        heap_free(entry.second);
    host->handle_tokens.clear();
    host->handle_reverse.clear();
}

static void efx_reconcile_refs(std::set<uint32_t> &current, const std::set<uint32_t> &desired) {
    for (uint32_t id : current)
        if (!desired.count(id))
            if (ComObj *o = com_get(id))
                com_release(o);
    for (uint32_t id : desired)
        if (!current.count(id))
            if (ComObj *o = com_get(id))
                com_addref(o);
    current = desired;
}

static bool efx_texture_ref_allowed(uint32_t id, uint32_t device_id, bool check_device) {
    if (!id)
        return false;
    ComObj *o = com_get(id);
    return o && efx_texture_kind(o->kind) && (!check_device || o->dev_d3d == device_id);
}

static void efx_reconcile_texture_ref(std::set<uint32_t> &refs, uint32_t id, uint32_t count) {
    if (!id)
        return;
    if (count) {
        if (refs.insert(id).second)
            if (ComObj *o = com_get(id))
                com_addref(o);
    } else if (refs.erase(id)) {
        if (ComObj *o = com_get(id))
            com_release(o);
    }
}

static void efx_update_texture_refs(ComObj *fx, uint32_t handle, uint32_t old_id, uint32_t new_id) {
    D3dxEffectHost *host = efx_host(fx);
    if (!host || old_id == new_id)
        return;
    if (recomp::d3d::d3dx_effect_texture_shared(*host->value, handle) && host->pool_obj) {
        if (ComObj *pool = com_get(host->pool_obj); pool && pool->d3dx_effect_pool) {
            uint32_t old_count = recomp::d3d::d3dx_effect_pool_texture_ref_count(
                *pool->d3dx_effect_pool->value, old_id);
            uint32_t new_count = recomp::d3d::d3dx_effect_pool_texture_ref_count(
                *pool->d3dx_effect_pool->value, new_id);
            if (!efx_texture_ref_allowed(new_id, 0, false))
                new_count = 0;
            efx_reconcile_texture_ref(pool->d3dx_effect_pool->texture_refs, old_id, old_count);
            efx_reconcile_texture_ref(pool->d3dx_effect_pool->texture_refs, new_id, new_count);
            return;
        }
    }
    uint32_t old_count = recomp::d3d::d3dx_effect_texture_ref_count(*host->value, old_id);
    uint32_t new_count = recomp::d3d::d3dx_effect_texture_ref_count(*host->value, new_id);
    if (!efx_texture_ref_allowed(new_id, host->device_id, true))
        new_count = 0;
    efx_reconcile_texture_ref(host->texture_refs, old_id, old_count);
    efx_reconcile_texture_ref(host->texture_refs, new_id, new_count);
}

static void efx_refresh_textures(ComObj *fx) {
    D3dxEffectHost *host = efx_host(fx);
    if (!host)
        return;
    std::set<uint32_t> desired;
    for (uint32_t id : recomp::d3d::d3dx_effect_texture_refs(*host->value)) {
        if (!id)
            continue;
        ComObj *o = com_get(id);
        if (o && efx_texture_kind(o->kind) && o->dev_d3d == host->device_id)
            desired.insert(id);
    }
    efx_reconcile_refs(host->texture_refs, desired);
}

static void efx_refresh_pool_textures(ComObj *pool) {
    if (!pool || !pool->d3dx_effect_pool)
        return;
    std::set<uint32_t> desired;
    for (uint32_t id : recomp::d3d::d3dx_effect_pool_texture_refs(*pool->d3dx_effect_pool->value)) {
        if (!id)
            continue;
        ComObj *o = com_get(id);
        if (o && efx_texture_kind(o->kind))
            desired.insert(id);
    }
    efx_reconcile_refs(pool->d3dx_effect_pool->texture_refs, desired);
}

static void efx_refresh_saved(ComObj *dev, ComObj *fx) {
    D3dxEffectHost *host = efx_host(fx);
    if (!host || !dev || !dev->d3d9_device)
        return;
    recomp::d3d::D3d9BindingsResult result = recomp::d3d::d3dx_effect_saved_bindings(*host->value);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_reconcile_refs(host->saved_refs, {});
        return;
    }
    efx_reconcile_refs(host->saved_refs, d3d9_binding_set(dev, result.live));
}

static void efx_drop_saved(ComObj *fx) {
    D3dxEffectHost *host = efx_host(fx);
    if (host)
        efx_reconcile_refs(host->saved_refs, {});
}

static recomp::d3d::Status efx_publish_shaders(ComObj *dev, ComObj *fx) {
    D3dxEffectHost *host = efx_host(fx);
    recomp::d3d::D3dxEffectShadersResult result =
        recomp::d3d::d3dx_effect_prepare_shaders(*host->value, *dev->d3d9_device->value);
    if (result.status.code != recomp::d3d::StatusCode::Ok)
        return result.status;
    std::set<uint32_t> added;
    auto publish = [&](uint32_t handle, bool pixel) -> bool {
        if (!handle || dev->d3d9_device->shader_views.count(handle))
            return true;
        ComObj *o = com_new(pixel ? K_D3D9PS : K_D3D9VS);
        if (!o)
            return false;
        o->dev_d3d = dev->id;
        o->d3d9_state_handle = handle;
        com_internalize(o);
        dev->d3d9_device->shader_views[handle] = o->id;
        host->published_shaders.insert(o->id);
        added.insert(o->id);
        return true;
    };
    bool published = true;
    for (uint32_t handle : result.vertex)
        if (!publish(handle, false)) {
            published = false;
            break;
        }
    if (published)
        for (uint32_t handle : result.pixel)
            if (!publish(handle, true)) {
                published = false;
                break;
            }
    if (!published) {
        for (uint32_t handle : result.vertex)
            if (!dev->d3d9_device->shader_views.count(handle))
                dev->d3d9_device->destroy_shader(false, handle);
        for (uint32_t handle : result.pixel)
            if (!dev->d3d9_device->shader_views.count(handle))
                dev->d3d9_device->destroy_shader(true, handle);
        for (uint32_t id : added)
            if (ComObj *o = com_get(id)) {
                if (o->d3d9_state_handle)
                    dev->d3d9_device->shader_views.erase(o->d3d9_state_handle);
                host->published_shaders.erase(id);
                com_release_internal(o);
            }
        return recomp::d3d::Status{recomp::d3d::StatusCode::OutOfMemory,
                                   rust::String("ID3DXEffect: shader view allocation failed")};
    }
    return recomp::d3d::Status{recomp::d3d::StatusCode::Ok, rust::String()};
}

static const uint32_t EFX_MANAGER_ADDREF = 1 * 4;
static const uint32_t EFX_MANAGER_RELEASE = 2 * 4;
static const uint32_t EFX_MANAGER_SET_TRANSFORM = 3 * 4;
static const uint32_t EFX_MANAGER_SET_RENDER_STATE = 7 * 4;
static const uint32_t EFX_MANAGER_SET_TEXTURE = 8 * 4;
static const uint32_t EFX_MANAGER_SET_TEXTURE_STAGE_STATE = 9 * 4;
static const uint32_t EFX_MANAGER_SET_SAMPLER_STATE = 10 * 4;
static const uint32_t EFX_MANAGER_SET_FVF = 12 * 4;
static const uint32_t EFX_MANAGER_SET_VERTEX_SHADER = 13 * 4;
static const uint32_t EFX_MANAGER_SET_VERTEX_SHADER_CONSTANT_F = 14 * 4;
static const uint32_t EFX_MANAGER_SET_PIXEL_SHADER = 17 * 4;
static const uint32_t EFX_MANAGER_SET_PIXEL_SHADER_CONSTANT_F = 18 * 4;

static bool efx_manager_valid(uint32_t manager) {
    if (!manager || !gm_valid(manager, 4))
        return false;
    uint32_t vtbl = rd32(manager);
    return vtbl && gm_valid(vtbl, 21 * 4);
}

static uint32_t efx_manager_method(uint32_t manager, uint32_t offset, const char *name) {
    uint32_t vtbl = efx_manager_valid(manager) ? rd32(manager) : 0;
    if (!vtbl || !gm_valid(vtbl + offset, 4)) {
        LOGW("d3d9: ID3DXEffectStateManager::%s vtable slot is not readable", name);
        return 0;
    }
    uint32_t fn = rd32(vtbl + offset);
    if (!fn || !gm_valid(fn, 1)) {
        LOGW("d3d9: ID3DXEffectStateManager::%s target is not a readable guest address", name);
        return 0;
    }
    return fn;
}

void d3dx_effect_destroy(ComObj *o) {
    if (!o->d3dx_effect)
        return;
    D3dxEffectHost *host = o->d3dx_effect.get();
    for (auto &entry : host->strings)
        heap_free(entry.second);
    host->strings.clear();
    for (auto &entry : host->pass_shaders) {
        if (entry.second.first)
            heap_free(entry.second.first);
        if (entry.second.second)
            heap_free(entry.second.second);
    }
    host->pass_shaders.clear();
    if (host->dispatch_scratch)
        heap_free(host->dispatch_scratch);
    host->dispatch_scratch = 0;
    host->dispatch_scratch_size = 0;
    efx_free_handles(host);
    efx_reconcile_refs(host->texture_refs, {});
    efx_reconcile_refs(host->saved_refs, {});
    if (host->state_manager) {
        X86 *c = guest_current_context();
        if (!c) {
            LOGW("d3d9: ID3DXEffectStateManager::Release requires a guest context");
            std::abort();
        } else {
            uint32_t release =
                efx_manager_method(host->state_manager, EFX_MANAGER_RELEASE, "Release");
            if (!release) {
                LOGW("d3d9: ID3DXEffectStateManager::Release unavailable during effect destroy");
                std::abort();
            } else
                guest_call(c, release, host->state_manager);
        }
        host->state_manager = 0;
    }
    ComObj *pool = com_get(host->pool_obj);
    ComObj *dev = com_get(host->device_id);
    if (dev && dev->d3d9_device) {
        recomp::d3d::d3dx_effect_release(*host->value);
        recomp::d3d::d3dx_effect_shutdown(*host->value, *dev->d3d9_device->value);
    }
    for (uint32_t id : host->published_shaders)
        if (ComObj *shader = com_get(id))
            com_release_internal(shader);
    host->published_shaders.clear();
    if (pool && pool->d3dx_effect_pool)
        efx_refresh_pool_textures(pool);
    if (pool)
        com_release(pool);
    if (dev)
        com_release(dev);
    o->d3dx_effect.reset();
}

void d3dx_effect_pool_destroy(ComObj *o) {
    if (!o || !o->d3dx_effect_pool)
        return;
    efx_reconcile_refs(o->d3dx_effect_pool->texture_refs, {});
    o->d3dx_effect_pool.reset();
}

void X_D3DX33CreateEffectPool(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    auto host = std::make_shared<D3dxEffectPoolHost>();
    ComObj *o = com_new(K_D3DXEFFECTPOOL);
    if (!o) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    o->d3dx_effect_pool = host;
    uint32_t view = com_view(o, IF_D3DXEFFECTPOOL33);
    if (!view) {
        com_release(o);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void efx_create(X86 *c, ComObj *dev, uint32_t pool_addr, uint32_t flags, uint32_t out,
                const uint8_t *bytes, size_t size) {
    ComObj *pool = pool_addr ? com_this(pool_addr) : nullptr;
    if (pool_addr && (!pool || pool->kind != K_D3DXEFFECTPOOL || !pool->d3dx_effect_pool)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (flags & ~EFX_KNOWN_CREATION_FLAGS) {
        com_ret(c, bridge_result(
                       c, recomp::d3d::Status{recomp::d3d::StatusCode::Unsupported,
                                              rust::String("D3DXCreateEffect: "
                                                           "unknown effect creation flags")}));
        return;
    }
    auto host = std::make_shared<D3dxEffectHost>();
    host->device_id = dev->id;
    recomp::d3d::D3dxEffectCreateResult result = recomp::d3d::d3dx_effect_create(
        *host->value, *dev->d3d9_device->value, rust::Slice<const uint8_t>(bytes, size), flags);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_addref(dev);
    if (pool) {
        recomp::d3d::Status join = recomp::d3d::d3dx_effect_join_pool(
            *host->value, *pool->d3dx_effect_pool->value, dev->id);
        if (join.code != recomp::d3d::StatusCode::Ok) {
            recomp::d3d::d3dx_effect_release(*host->value);
            com_release(dev);
            com_ret(c, bridge_result(c, join));
            return;
        }
        com_addref(pool);
        host->pool_obj = pool->id;
    }
    ComObj *o = com_new(K_D3DXEFFECT);
    if (!o) {
        recomp::d3d::d3dx_effect_release(*host->value);
        if (pool)
            com_release(pool);
        com_release(dev);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    o->d3dx_effect = host;
    uint32_t view = com_view(o, IF_D3DXEFFECT33);
    if (!view) {
        com_release(o);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    efx_refresh_textures(o);
    if (pool)
        efx_refresh_pool_textures(pool);
    recomp::d3d::Status publish = efx_publish_shaders(dev, o);
    if (publish.code != recomp::d3d::StatusCode::Ok) {
        com_release(o);
        com_ret(c, bridge_result(c, publish));
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void X_D3DXCreateEffectFromFileA(X86 *c) {
    uint32_t device_addr = arg(c, 0), file = arg(c, 1), flags = arg(c, 4);
    uint32_t pool_addr = arg(c, 5), out = arg(c, 6), errors = arg(c, 7);
    if (errors && gm_valid(errors, 4))
        wr32(errors, 0);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    ComObj *dev = com_this(device_addr);
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device || !file) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::string path = win32_host_path_op(gm_str(file, 1024), WIN32_FILE_READ);
    std::vector<uint8_t> bytes;
    if (!d3dx_read_file(path, bytes)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_create(c, dev, pool_addr, flags, out, bytes.data(), bytes.size());
}

void Efx_GetDesc(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t desc = arg(c, 1);
    if (!fx || !desc || !gm_valid(desc, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxEffectDescResult result = recomp::d3d::d3dx_effect_desc(*efx_host(fx)->value);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    wr32(desc + 0, 0);
    wr32(desc + 4, result.desc.parameters);
    wr32(desc + 8, result.desc.techniques);
    wr32(desc + 12, result.desc.functions);
    com_ret(c, D3D_OK9);
}

void Efx_GetParameterDesc(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t desc = arg(c, 2);
    if (!fx || !desc || !gm_valid(desc, 44)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxEffectParamDescResult result = recomp::d3d::d3dx_effect_param_desc(
        *efx_host(fx)->value, efx_resolve_handle(fx, arg(c, 1), 0));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    std::vector<uint8_t> name(result.name.begin(), result.name.end());
    std::vector<uint8_t> semantic(result.semantic.begin(), result.semantic.end());
    uint32_t name_addr = name.empty() ? 0 : efx_guest_string(efx_host(fx), name);
    if (!name.empty() && !name_addr) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    uint32_t semantic_addr = semantic.empty() ? 0 : efx_guest_string(efx_host(fx), semantic);
    if (!semantic.empty() && !semantic_addr) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(desc + 0, name_addr);
    wr32(desc + 4, semantic_addr);
    wr32(desc + 8, result.desc.class_);
    wr32(desc + 12, result.desc.type_);
    wr32(desc + 16, result.desc.rows);
    wr32(desc + 20, result.desc.columns);
    wr32(desc + 24, result.desc.elements);
    wr32(desc + 28, result.desc.annotations);
    wr32(desc + 32, result.desc.struct_members);
    wr32(desc + 36, result.desc.flags);
    wr32(desc + 40, result.desc.bytes);
    com_ret(c, D3D_OK9);
}

void Efx_GetTechniqueDesc(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t desc = arg(c, 2);
    if (!fx || !desc || !gm_valid(desc, 12)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxEffectTechniqueDescResult result = recomp::d3d::d3dx_effect_technique_desc(
        *efx_host(fx)->value, efx_resolve_handle(fx, arg(c, 1), 1));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    std::vector<uint8_t> name(result.name.begin(), result.name.end());
    uint32_t name_addr = name.empty() ? 0 : efx_guest_string(efx_host(fx), name);
    if (!name.empty() && !name_addr) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(desc + 0, name_addr);
    wr32(desc + 4, result.desc.passes);
    wr32(desc + 8, result.desc.annotations);
    com_ret(c, D3D_OK9);
}

void Efx_GetPassDesc(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t desc = arg(c, 2);
    if (!fx || !desc || !gm_valid(desc, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    D3dxEffectHost *host = efx_host(fx);
    uint32_t pass = efx_resolve_handle(fx, arg(c, 1), 2);
    recomp::d3d::D3dxEffectPassDescResult result =
        recomp::d3d::d3dx_effect_pass_desc(*host->value, pass);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    std::vector<uint8_t> name(result.name.begin(), result.name.end());
    uint32_t name_addr = name.empty() ? 0 : efx_guest_string(host, name);
    if (!name.empty() && !name_addr) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(desc + 0, name_addr);
    wr32(desc + 4, result.desc.annotations);
    auto it = host->pass_shaders.find(pass);
    if (it == host->pass_shaders.end()) {
        auto copy_shader = [](const rust::Vec<uint8_t> &bytes) -> uint32_t {
            if (bytes.empty())
                return 0;
            uint32_t addr = heap_alloc((uint32_t)bytes.size(), false, 16);
            if (addr)
                memcpy(gm_ptr(addr), bytes.data(), bytes.size());
            return addr;
        };
        uint32_t vertex = copy_shader(result.desc.vertex_shader);
        uint32_t pixel = copy_shader(result.desc.pixel_shader);
        if ((!result.desc.vertex_shader.empty() && !vertex) ||
            (!result.desc.pixel_shader.empty() && !pixel)) {
            if (vertex)
                heap_free(vertex);
            if (pixel)
                heap_free(pixel);
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
        it = host->pass_shaders.emplace(pass, std::make_pair(vertex, pixel)).first;
    }
    wr32(desc + 8, it->second.first);
    wr32(desc + 12, it->second.second);
    com_ret(c, D3D_OK9);
}

void Efx_GetParameter(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        set_eax(c, 0);
        return;
    }
    uint32_t parent = efx_resolve_handle(fx, arg(c, 1), 0);
    set_eax(c, efx_wrap_handle(
                   fx, recomp::d3d::d3dx_effect_enum(*efx_host(fx)->value, parent, arg(c, 2), 0)));
}

void Efx_GetParameterElement(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        set_eax(c, 0);
        return;
    }
    uint32_t parent = efx_resolve_handle(fx, arg(c, 1), 0);
    set_eax(c, efx_wrap_handle(
                   fx, recomp::d3d::d3dx_effect_enum(*efx_host(fx)->value, parent, arg(c, 2), 4)));
}

void Efx_GetParameterByName(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t name = arg(c, 2);
    if (!fx || !name) {
        set_eax(c, 0);
        return;
    }
    uint32_t parent = efx_resolve_handle(fx, arg(c, 1), 0);
    std::string text = gm_str(name, 16384);
    set_eax(c,
            efx_wrap_handle(
                fx, recomp::d3d::d3dx_effect_find(
                        *efx_host(fx)->value, parent,
                        rust::Slice<const uint8_t>((const uint8_t *)text.data(), text.size()), 0)));
}

void Efx_GetParameterBySemantic(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t semantic = arg(c, 2);
    set_eax(c, 0);
    if (!fx || !semantic)
        return;
    std::string want = gm_str(semantic, 16384);
    D3dxEffectHost *host = efx_host(fx);
    uint32_t parent = efx_resolve_handle(fx, arg(c, 1), 0);
    for (uint32_t index = 0;; ++index) {
        uint32_t handle = recomp::d3d::d3dx_effect_enum(*host->value, parent, index, 0);
        if (!handle)
            break;
        recomp::d3d::D3dxBytesResult name =
            recomp::d3d::d3dx_effect_param_semantic(*host->value, handle);
        std::string text(name.bytes.begin(), name.bytes.end());
        if (text == want) {
            set_eax(c, efx_wrap_handle(fx, handle));
            return;
        }
    }
}

void Efx_GetTechnique(X86 *c) {
    ComObj *fx = efx_this(c);
    set_eax(c, fx ? efx_wrap_handle(
                        fx, recomp::d3d::d3dx_effect_enum(*efx_host(fx)->value, 0, arg(c, 1), 1))
                  : 0);
}

void Efx_GetTechniqueByName(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t name = arg(c, 1);
    if (!fx || !name) {
        set_eax(c, 0);
        return;
    }
    std::string text = gm_str(name, 16384);
    set_eax(c,
            efx_wrap_handle(
                fx, recomp::d3d::d3dx_effect_find(
                        *efx_host(fx)->value, 0,
                        rust::Slice<const uint8_t>((const uint8_t *)text.data(), text.size()), 1)));
}

void Efx_GetPass(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        set_eax(c, 0);
        return;
    }
    uint32_t technique = efx_resolve_handle(fx, arg(c, 1), 1);
    set_eax(c, efx_wrap_handle(fx, recomp::d3d::d3dx_effect_enum(*efx_host(fx)->value, technique,
                                                                 arg(c, 2), 2)));
}

void Efx_GetPassByName(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t name = arg(c, 2);
    if (!fx || !name) {
        set_eax(c, 0);
        return;
    }
    uint32_t technique = efx_resolve_handle(fx, arg(c, 1), 1);
    std::string text = gm_str(name, 16384);
    set_eax(c,
            efx_wrap_handle(
                fx, recomp::d3d::d3dx_effect_find(
                        *efx_host(fx)->value, technique,
                        rust::Slice<const uint8_t>((const uint8_t *)text.data(), text.size()), 2)));
}

void Efx_GetAnnotation(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        set_eax(c, 0);
        return;
    }
    uint32_t object = efx_resolve_handle(fx, arg(c, 1), 3);
    set_eax(c, efx_wrap_handle(
                   fx, recomp::d3d::d3dx_effect_enum(*efx_host(fx)->value, object, arg(c, 2), 3)));
}

void Efx_GetAnnotationByName(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t name = arg(c, 2);
    if (!fx || !name) {
        set_eax(c, 0);
        return;
    }
    uint32_t object = efx_resolve_handle(fx, arg(c, 1), 3);
    std::string text = gm_str(name, 16384);
    set_eax(c,
            efx_wrap_handle(
                fx, recomp::d3d::d3dx_effect_find(
                        *efx_host(fx)->value, object,
                        rust::Slice<const uint8_t>((const uint8_t *)text.data(), text.size()), 3)));
}

static void efx_apply(X86 *c, uint32_t setter, const uint8_t *data, size_t size) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t handle = efx_resolve_handle(fx, arg(c, 1), 0);
    efx_status(c, recomp::d3d::d3dx_effect_set(*efx_host(fx)->value, handle, setter,
                                               rust::Slice<const uint8_t>(data, size)));
}

static void efx_set_array_n(X86 *c, uint32_t setter, uint32_t element, uint32_t count) {
    uint32_t addr = arg(c, 2);
    if (!addr || !count || !gm_fits_n(addr, count, element)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_apply(c, setter, (const uint8_t *)gm_ptr(addr), (size_t)count * element);
}

static void efx_set_array(X86 *c, uint32_t setter, uint32_t element) {
    efx_set_array_n(c, setter, element, arg(c, 3));
}

void Efx_SetValue(X86 *c) {
    uint32_t addr = arg(c, 2), size = arg(c, 3);
    if (size && (!addr || !gm_valid(addr, size))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_apply(c, 0, size ? (const uint8_t *)gm_ptr(addr) : nullptr, size);
}

void Efx_SetRawValue(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t addr = arg(c, 2), offset = arg(c, 3), bytes = arg(c, 4);
    if (!fx || (bytes && (!addr || !gm_valid(addr, bytes)))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t handle = efx_resolve_handle(fx, arg(c, 1), 0);
    efx_status(
        c, recomp::d3d::d3dx_effect_set_raw(
               *efx_host(fx)->value, handle, offset,
               rust::Slice<const uint8_t>(bytes ? (const uint8_t *)gm_ptr(addr) : nullptr, bytes)));
}

void Efx_SetBool(X86 *c) {
    uint32_t value = arg(c, 2) ? 1 : 0;
    efx_apply(c, 1, (const uint8_t *)&value, 4);
}
void Efx_SetBoolArray(X86 *c) {
    efx_set_array(c, 2, 4);
}
void Efx_SetInt(X86 *c) {
    uint32_t value = arg(c, 2);
    efx_apply(c, 3, (const uint8_t *)&value, 4);
}
void Efx_SetIntArray(X86 *c) {
    efx_set_array(c, 4, 4);
}
void Efx_SetFloat(X86 *c) {
    uint32_t value = arg(c, 2);
    efx_apply(c, 5, (const uint8_t *)&value, 4);
}
void Efx_SetFloatArray(X86 *c) {
    efx_set_array(c, 6, 4);
}
void Efx_SetVector(X86 *c) {
    efx_set_array_n(c, 7, 16, 1);
}
void Efx_SetVectorArray(X86 *c) {
    efx_set_array(c, 8, 16);
}
void Efx_SetMatrix(X86 *c) {
    efx_set_array_n(c, 9, 64, 1);
}
void Efx_SetMatrixArray(X86 *c) {
    efx_set_array(c, 10, 64);
}
void Efx_SetMatrixTranspose(X86 *c) {
    efx_set_array_n(c, 12, 64, 1);
}
void Efx_SetMatrixTransposeArray(X86 *c) {
    efx_set_array(c, 13, 64);
}

static void efx_set_pointer_array(X86 *c, uint32_t setter) {
    uint32_t addr = arg(c, 2), count = arg(c, 3);
    if (!addr || !count || !gm_fits_n(addr, count, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<uint8_t> gathered;
    gathered.reserve((size_t)count * 64);
    for (uint32_t i = 0; i < count; ++i) {
        uint32_t pointer = rd32(addr + i * 4);
        if (!pointer || !gm_valid(pointer, 64)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        const uint8_t *bytes = (const uint8_t *)gm_ptr(pointer);
        gathered.insert(gathered.end(), bytes, bytes + 64);
    }
    efx_apply(c, setter, gathered.data(), gathered.size());
}

void Efx_SetMatrixPointerArray(X86 *c) {
    efx_set_pointer_array(c, 11);
}
void Efx_SetMatrixTransposePointerArray(X86 *c) {
    efx_set_pointer_array(c, 14);
}

static void efx_get(X86 *c, uint32_t kind, uint64_t limit) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t addr = arg(c, 2);
    uint32_t handle = efx_resolve_handle(fx, arg(c, 1), 0);
    recomp::d3d::D3dxBytesResult result =
        recomp::d3d::d3dx_effect_get(*efx_host(fx)->value, handle, kind);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    size_t bytes = std::min<size_t>(result.bytes.size(), (size_t)limit);
    if (bytes && (!addr || !gm_valid(addr, (uint32_t)bytes))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (bytes)
        memcpy(gm_ptr(addr), result.bytes.data(), bytes);
    com_ret(c, D3D_OK9);
}

static uint64_t efx_array_bytes(uint32_t count, uint32_t element) {
    return (uint64_t)count * element;
}

void Efx_GetValue(X86 *c) {
    efx_get(c, 3, arg(c, 3));
}
void Efx_GetBool(X86 *c) {
    efx_get(c, 2, 4);
}
void Efx_GetInt(X86 *c) {
    efx_get(c, 1, 4);
}
void Efx_GetFloat(X86 *c) {
    efx_get(c, 0, 4);
}
void Efx_GetBoolArray(X86 *c) {
    efx_get(c, 6, efx_array_bytes(arg(c, 3), 4));
}
void Efx_GetIntArray(X86 *c) {
    efx_get(c, 5, efx_array_bytes(arg(c, 3), 4));
}
void Efx_GetFloatArray(X86 *c) {
    efx_get(c, 4, efx_array_bytes(arg(c, 3), 4));
}
void Efx_GetVector(X86 *c) {
    efx_get(c, 7, 16);
}
void Efx_GetVectorArray(X86 *c) {
    efx_get(c, 8, efx_array_bytes(arg(c, 3), 16));
}
void Efx_GetMatrix(X86 *c) {
    efx_get(c, 9, 64);
}
void Efx_GetMatrixArray(X86 *c) {
    efx_get(c, 10, efx_array_bytes(arg(c, 3), 64));
}
void Efx_GetMatrixTranspose(X86 *c) {
    efx_get(c, 11, 64);
}
void Efx_GetMatrixTransposeArray(X86 *c) {
    efx_get(c, 12, efx_array_bytes(arg(c, 3), 64));
}

void Efx_GetString(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 2);
    if (!fx || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3dxBytesResult result = recomp::d3d::d3dx_effect_get(
        *efx_host(fx)->value, efx_resolve_handle(fx, arg(c, 1), 0), 13);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    std::vector<uint8_t> bytes(result.bytes.begin(), result.bytes.end());
    uint32_t addr = bytes.empty() ? 0 : efx_guest_string(efx_host(fx), bytes);
    if (!bytes.empty() && !addr) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(out, addr);
    com_ret(c, D3D_OK9);
}

void Efx_SetTexture(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t handle = efx_resolve_handle(fx, arg(c, 1), 0);
    uint32_t texture = arg(c, 2);
    ComObj *tex = texture ? com_this(texture) : nullptr;
    if (texture && !tex) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (tex && (!efx_texture_kind(tex->kind) || tex->dev_d3d != fx->d3dx_effect->device_id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t id = tex ? tex->id : 0;
    recomp::d3d::ValueResult result =
        recomp::d3d::d3dx_effect_set_texture(*efx_host(fx)->value, handle, id);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    if (result.value == id) {
        com_ret(c, D3D_OK9);
        return;
    }
    efx_update_texture_refs(fx, handle, result.value, id);
    com_ret(c, D3D_OK9);
}

void Efx_GetTexture(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 2);
    if (!fx || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t handle = efx_resolve_handle(fx, arg(c, 1), 0);
    recomp::d3d::ValueResult result =
        recomp::d3d::d3dx_effect_texture(*efx_host(fx)->value, handle);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    ComObj *tex = com_get(result.value);
    if (!tex || !efx_texture_kind(tex->kind) || tex->dev_d3d != fx->d3dx_effect->device_id) {
        com_out_ptr(out, 0);
        com_ret(c, D3D_OK9);
        return;
    }
    uint32_t view = com_view(tex, IF_D3DBASETEXTURE9);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(tex);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void Efx_GetPool(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 1);
    if (!fx || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *pool = com_get(efx_host(fx)->pool_obj);
    if (!pool) {
        com_out_ptr(out, 0);
        com_ret(c, D3D_OK9);
        return;
    }
    uint32_t view = com_view(pool, IF_D3DXEFFECTPOOL33);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(pool);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void Efx_SetTechnique(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_status(c, recomp::d3d::d3dx_effect_set_technique(*efx_host(fx)->value,
                                                         efx_resolve_handle(fx, arg(c, 1), 1)));
}

void Efx_GetCurrentTechnique(X86 *c) {
    ComObj *fx = efx_this(c);
    set_eax(c, fx ? efx_wrap_handle(
                        fx, recomp::d3d::d3dx_effect_current_technique(*efx_host(fx)->value))
                  : 0);
}

void Efx_ValidateTechnique(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_status(c, recomp::d3d::d3dx_effect_validate_technique(
                      *efx_host(fx)->value, efx_resolve_handle(fx, arg(c, 1), 1)));
}

void Efx_FindNextValidTechnique(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 2);
    if (!fx || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t next = recomp::d3d::d3dx_effect_find_next_valid(*efx_host(fx)->value,
                                                             efx_resolve_handle(fx, arg(c, 1), 1));
    uint32_t wrapped = efx_wrap_handle(fx, next);
    if (next && !wrapped) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    wr32(out, wrapped);
    com_ret(c, next ? D3D_OK9 : 1u);
}

void Efx_IsParameterUsed(X86 *c) {
    ComObj *fx = efx_this(c);
    set_eax(c, fx ? recomp::d3d::d3dx_effect_is_parameter_used(*efx_host(fx)->value,
                                                               efx_resolve_handle(fx, arg(c, 1), 0),
                                                               efx_resolve_handle(fx, arg(c, 2), 1))
                  : 0);
}

static bool efx_hr_failed(uint32_t hr) {
    return (int32_t)hr < 0;
}

static uint32_t efx_dispatch_commands(X86 *c, ComObj *fx, ComObj *dev,
                                      const rust::Vec<recomp::d3d::D3dxEffectCommand> &commands,
                                      const rust::Vec<float> &values) {
    D3dxEffectHost *host = efx_host(fx);
    if (!host || !dev || !dev->d3d9_device)
        return D3DERR_INVALIDCALL;
    const size_t command_count = commands.size();
    if (command_count == 0)
        return D3D_OK9;
    uint32_t manager = host->state_manager;
    if (!efx_manager_valid(manager)) {
        LOGW("d3d9: ID3DXEffect needs a valid ID3DXEffectStateManager for %u commands",
             (unsigned)command_count);
        return D3DERR_INVALIDCALL;
    }
    const recomp::d3d::D3dxEffectCommand *cmd_list = commands.data();
    uint32_t resolved_inline[128];
    std::vector<uint32_t> resolved_heap;
    uint32_t *resolved = resolved_inline;
    if (command_count > std::size(resolved_inline)) {
        resolved_heap.resize(command_count);
        resolved = resolved_heap.data();
    }
    std::fill(resolved, resolved + command_count, 0u);
    uint32_t pinned_inline[32];
    std::vector<uint32_t> pinned_heap;
    size_t pinned_count = 0;
    auto pinned_at = [&](size_t i) {
        return i < std::size(pinned_inline) ? pinned_inline[i]
                                            : pinned_heap[i - std::size(pinned_inline)];
    };
    auto pin = [&](uint32_t id) {
        for (size_t i = 0; i < pinned_count; ++i)
            if (pinned_at(i) == id)
                return false;
        if (pinned_count < std::size(pinned_inline))
            pinned_inline[pinned_count] = id;
        else
            pinned_heap.push_back(id);
        ++pinned_count;
        return true;
    };
    bool stale = false;
    bool oom = false;
    for (size_t i = 0; i < command_count && !stale && !oom; ++i) {
        const recomp::d3d::D3dxEffectCommand &cmd = cmd_list[i];
        if (cmd.kind == recomp::d3d::D3dxEffectCommandKind::SetTexture) {
            if (!cmd.value)
                continue;
            ComObj *o = com_get(cmd.value);
            if (!o || !efx_texture_kind(o->kind) || o->dev_d3d != host->device_id) {
                LOGW("d3d9: ID3DXEffectStateManager::SetTexture stale texture %u", cmd.value);
                stale = true;
                break;
            }
            resolved[i] = com_view(o, IF_D3DBASETEXTURE9);
            if (!resolved[i]) {
                oom = true;
                break;
            }
            if (pin(cmd.value))
                com_addref(o);
        } else if (cmd.kind == recomp::d3d::D3dxEffectCommandKind::SetVertexShader ||
                   cmd.kind == recomp::d3d::D3dxEffectCommandKind::SetPixelShader) {
            if (!cmd.value)
                continue;
            bool vertex = cmd.kind == recomp::d3d::D3dxEffectCommandKind::SetVertexShader;
            auto it = dev->d3d9_device->shader_views.find(cmd.value);
            ComObj *o = it == dev->d3d9_device->shader_views.end() ? nullptr : com_get(it->second);
            if (!o || o->kind != (vertex ? K_D3D9VS : K_D3D9PS) || o->dev_d3d != dev->id) {
                LOGW("d3d9: ID3DXEffectStateManager::%s stale shader handle %u",
                     vertex ? "SetVertexShader" : "SetPixelShader", cmd.value);
                stale = true;
                break;
            }
            resolved[i] = com_view(o, vertex ? IF_D3DVERTEXSHADER9 : IF_D3DPIXELSHADER9);
            if (!resolved[i]) {
                oom = true;
                break;
            }
            if (pin(it->second))
                com_addref(o);
        }
    }

    auto unpin = [&]() {
        for (size_t i = 0; i < pinned_count; ++i)
            if (ComObj *o = com_get(pinned_at(i)))
                com_release(o);
    };

    if (stale || oom) {
        unpin();
        return oom ? E_OUTOFMEMORY : D3DERR_INVALIDCALL;
    }

    auto temporary = [&](uint32_t bytes) -> uint32_t {
        if (!bytes)
            return 0;
        if (bytes > host->dispatch_scratch_size) {
            if (host->dispatch_scratch)
                heap_free(host->dispatch_scratch);
            host->dispatch_scratch = heap_alloc(bytes, false, 16);
            host->dispatch_scratch_size = host->dispatch_scratch ? bytes : 0;
        }
        return host->dispatch_scratch;
    };

    struct ManagerMethod {
        uint32_t fn = 0;
        RecompHostCall host = nullptr;
        bool resolved = false;
    };
    ManagerMethod methods[EFX_MANAGER_SET_PIXEL_SHADER_CONSTANT_F / 4 + 1];
    auto method = [&](uint32_t offset, const char *name, uint32_t nargs) -> const ManagerMethod & {
        ManagerMethod &m = methods[offset / 4];
        if (!m.resolved) {
            m.fn = efx_manager_method(manager, offset, name);
            m.host = m.fn ? recomp_host_call(m.fn, nargs) : nullptr;
            m.resolved = true;
        }
        return m;
    };
    auto invoke = [&](uint32_t offset, const char *name,
                      std::initializer_list<uint32_t> args) -> uint32_t {
        const ManagerMethod &m = method(offset, name, (uint32_t)args.size());
        if (!m.fn) {
            stale = true;
            return D3D_OK9;
        }
        if (m.host)
            return m.host(c, args.begin());
        return guest_call(c, m.fn, args.begin(), (int)args.size());
    };
    const float *value_data = values.data();
    const size_t value_total = values.size();

    uint32_t failure = D3D_OK9;
    for (size_t i = 0; i < command_count && !oom; ++i) {
        const recomp::d3d::D3dxEffectCommand &cmd = cmd_list[i];
        uint32_t result = D3D_OK9;
        switch (cmd.kind) {
        case recomp::d3d::D3dxEffectCommandKind::SetRenderState:
            result = invoke(EFX_MANAGER_SET_RENDER_STATE, "SetRenderState",
                            {manager, cmd.state, cmd.value});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetTextureStageState:
            result = invoke(EFX_MANAGER_SET_TEXTURE_STAGE_STATE, "SetTextureStageState",
                            {manager, cmd.index, cmd.state, cmd.value});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetSamplerState:
            result = invoke(EFX_MANAGER_SET_SAMPLER_STATE, "SetSamplerState",
                            {manager, cmd.index, cmd.state, cmd.value});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetTransform: {
            if (cmd.count != 16 || (size_t)cmd.offset + 16 > value_total) {
                LOGW("d3d9: ID3DXEffectStateManager::SetTransform has an invalid matrix span");
                stale = true;
                break;
            }
            uint32_t buf = temporary(64);
            if (!buf) {
                oom = true;
                break;
            }
            memcpy(gm_ptr(buf), value_data + cmd.offset, 64);
            result = invoke(EFX_MANAGER_SET_TRANSFORM, "SetTransform", {manager, cmd.state, buf});
            break;
        }
        case recomp::d3d::D3dxEffectCommandKind::SetTexture:
            result =
                invoke(EFX_MANAGER_SET_TEXTURE, "SetTexture", {manager, cmd.index, resolved[i]});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetFVF:
            result = invoke(EFX_MANAGER_SET_FVF, "SetFVF", {manager, cmd.value});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetVertexShader:
            result =
                invoke(EFX_MANAGER_SET_VERTEX_SHADER, "SetVertexShader", {manager, resolved[i]});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetPixelShader:
            result = invoke(EFX_MANAGER_SET_PIXEL_SHADER, "SetPixelShader", {manager, resolved[i]});
            break;
        case recomp::d3d::D3dxEffectCommandKind::SetVertexShaderConstantF:
        case recomp::d3d::D3dxEffectCommandKind::SetPixelShaderConstantF: {
            bool vertex = cmd.kind == recomp::d3d::D3dxEffectCommandKind::SetVertexShaderConstantF;
            const char *name = vertex ? "SetVertexShaderConstantF" : "SetPixelShaderConstantF";
            if (cmd.count % 4 != 0 || (size_t)cmd.offset + cmd.count > value_total ||
                cmd.count > UINT32_MAX / 4) {
                LOGW("d3d9: ID3DXEffectStateManager::%s has an invalid constant span", name);
                stale = true;
                break;
            }
            uint32_t bytes = cmd.count * 4;
            uint32_t buf = temporary(bytes);
            if (bytes && !buf) {
                oom = true;
                break;
            }
            if (bytes)
                memcpy(gm_ptr(buf), value_data + cmd.offset, bytes);
            result = invoke(vertex ? EFX_MANAGER_SET_VERTEX_SHADER_CONSTANT_F
                                   : EFX_MANAGER_SET_PIXEL_SHADER_CONSTANT_F,
                            name, {manager, cmd.index, buf, cmd.count / 4});
            break;
        }
        default:
            LOGW("d3d9: ID3DXEffectStateManager command %d is unsupported", (int)cmd.kind);
            stale = true;
            break;
        }
        if (stale || oom)
            break;
        if (efx_hr_failed(result))
            failure = result;
    }

    unpin();
    if (oom)
        return E_OUTOFMEMORY;
    if (stale)
        return D3DERR_INVALIDCALL;
    return failure;
}

static bool d3d9_fvf_decl_view(ComObj *dev, uint32_t handle) {
    if (dev->d3d9_device->decl_views.find(handle) != dev->d3d9_device->decl_views.end())
        return true;
    ComObj *d = com_new(K_D3D9DECL);
    if (!d)
        return false;
    d->dev_d3d = dev->id;
    d->d3d9_state_handle = handle;
    com_internalize(d);
    if (!com_view(d, IF_D3DVERTEXDECL9)) {
        com_release_internal(d);
        return false;
    }
    dev->d3d9_device->decl_views[handle] = d->id;
    dev->d3d9_device->fvf_decls.insert(d->id);
    return true;
}

static uint32_t efx_run(X86 *c, ComObj *fx, ComObj *dev, recomp::d3d::D3dxEffectRunResult &result) {
    D3dxEffectHost *host = efx_host(fx);
    uint32_t hr;
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        if (result.reserved)
            recomp::d3d::d3dx_effect_batch_finish(*host->value, false);
        efx_refresh_textures(fx);
        if (ComObj *pool = com_get(host->pool_obj))
            efx_refresh_pool_textures(pool);
        d3d9_refresh_bindings(dev);
        hr = bridge_result(c, result.status);
    } else {
        hr = efx_dispatch_commands(c, fx, dev, result.commands, result.values);
        if (result.reserved)
            recomp::d3d::d3dx_effect_batch_finish(*host->value, !efx_hr_failed(hr));
        else {
            recomp::d3d::ValueResult decl = dev->d3d9_device->vertex_declaration();
            if (d3d_bridge_ok(decl.status) && decl.value)
                d3d9_fvf_decl_view(dev, decl.value);
            d3d9_refresh_bindings(dev);
        }
    }
    recomp::d3d::d3dx_effect_recycle(*host->value, std::move(result.commands),
                                     std::move(result.values));
    return hr;
}

void Efx_Begin(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    uint32_t passes = arg(c, 1);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (passes && !gm_valid(passes, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result =
        recomp::d3d::d3dx_effect_begin(*efx_host(fx)->value, *dev->d3d9_device->value, arg(c, 2));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        efx_status(c, result.status);
        return;
    }
    if (passes)
        wr32(passes, result.value);
    D3dxEffectHost *host = efx_host(fx);
    host->restores_state = !(arg(c, 2) & 1) && !host->state_manager;
    efx_refresh_saved(dev, fx);
    com_ret(c, D3D_OK9);
}

void Efx_BeginPass(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_addref(fx);
    com_addref(dev);
    recomp::d3d::D3dxEffectRunResult result = recomp::d3d::d3dx_effect_begin_pass(
        *efx_host(fx)->value, *dev->d3d9_device->value, arg(c, 1));
    uint32_t hr = efx_run(c, fx, dev, result);
    com_release(dev);
    com_release(fx);
    com_ret(c, hr);
}

void Efx_CommitChanges(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_addref(fx);
    com_addref(dev);
    recomp::d3d::D3dxEffectRunResult result =
        recomp::d3d::d3dx_effect_commit(*efx_host(fx)->value, *dev->d3d9_device->value);
    uint32_t hr = efx_run(c, fx, dev, result);
    com_release(dev);
    com_release(fx);
    com_ret(c, hr);
}

void Efx_EndPass(X86 *c) {
    ComObj *fx = efx_this(c);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    efx_status(c, recomp::d3d::d3dx_effect_end_pass(*efx_host(fx)->value));
}

void Efx_End(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    D3dxEffectHost *host = efx_host(fx);
    recomp::d3d::Status result =
        recomp::d3d::d3dx_effect_end(*host->value, *dev->d3d9_device->value);
    if (host->restores_state)
        d3d9_refresh_bindings(dev);
    host->restores_state = false;
    if (result.code == recomp::d3d::StatusCode::Ok)
        efx_drop_saved(fx);
    efx_status(c, result);
}

void Efx_GetDevice(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 1);
    ComObj *dev = fx ? com_get(fx->d3dx_effect->device_id) : nullptr;
    if (!fx || !dev || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(dev, IF_D3DDEVICE9);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(dev);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void Efx_OnLostDevice(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status result =
        recomp::d3d::d3dx_effect_on_lost(*efx_host(fx)->value, *dev->d3d9_device->value);
    efx_drop_saved(fx);
    efx_refresh_textures(fx);
    if (ComObj *pool = com_get(efx_host(fx)->pool_obj))
        efx_refresh_pool_textures(pool);
    d3d9_refresh_bindings(dev);
    efx_status(c, result);
}

void Efx_OnResetDevice(X86 *c) {
    ComObj *fx = efx_this(c);
    ComObj *dev = efx_device(c);
    if (!fx || !dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status result =
        recomp::d3d::d3dx_effect_on_reset(*efx_host(fx)->value, *dev->d3d9_device->value);
    efx_refresh_textures(fx);
    if (ComObj *pool = com_get(efx_host(fx)->pool_obj))
        efx_refresh_pool_textures(pool);
    efx_status(c, result);
}

void Efx_SetStateManager(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t manager = arg(c, 1);
    if (!fx) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    D3dxEffectHost *host = efx_host(fx);
    if (recomp::d3d::d3dx_effect_batch_active(*host->value)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (manager && !efx_manager_valid(manager)) {
        LOGW("d3d9: ID3DXEffectStateManager manager %#x is not a valid interface", manager);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t addref = manager ? efx_manager_method(manager, EFX_MANAGER_ADDREF, "AddRef") : 0;
    uint32_t manager_release =
        manager ? efx_manager_method(manager, EFX_MANAGER_RELEASE, "Release") : 0;
    uint32_t old = host->state_manager;
    uint32_t release = old ? efx_manager_method(old, EFX_MANAGER_RELEASE, "Release") : 0;
    if ((manager && (!addref || !manager_release)) || (old && !release)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status set =
        recomp::d3d::d3dx_effect_set_state_manager(*host->value, manager != 0);
    if (set.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, set));
        return;
    }
    com_addref(fx);
    if (manager)
        guest_call(c, addref, manager);
    host->state_manager = manager;
    if (old)
        guest_call(c, release, old);
    recomp::d3d::d3dx_effect_batch_finish(*host->value, true);
    if (manager)
        efx_drop_saved(fx);
    com_release(fx);
    com_ret(c, D3D_OK9);
}

void Efx_GetStateManager(X86 *c) {
    ComObj *fx = efx_this(c);
    uint32_t out = arg(c, 1);
    if (!fx || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    D3dxEffectHost *host = efx_host(fx);
    uint32_t manager = host->state_manager;
    if (manager) {
        uint32_t addref = efx_manager_method(manager, EFX_MANAGER_ADDREF, "AddRef");
        if (!addref) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        com_addref(fx);
        guest_call(c, addref, manager);
        wr32(out, manager);
        com_release(fx);
    } else {
        wr32(out, 0);
    }
    com_ret(c, D3D_OK9);
}

static uint32_t d3dx_profile_string(const std::string &profile) {
    if (profile.empty())
        return 0;
    auto &cache = d3dx_profile_strings();
    auto it = cache.find(profile);
    if (it != cache.end())
        return it->second;
    uint32_t addr = heap_alloc((uint32_t)profile.size() + 1, true, 16);
    if (!addr)
        return 0;
    gm_put_str(addr, profile.c_str(), (uint32_t)profile.size() + 1);
    cache[profile] = addr;
    return addr;
}

static void d3dx_shader_profile(X86 *c, bool vertex) {
    ComObj *dev = com_this(arg(c, 0));
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device) {
        set_eax(c, 0);
        return;
    }
    recomp::d3d::D3dxProfilesResult result =
        recomp::d3d::d3dx_shader_profiles(*dev->d3d9_device->value);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        set_eax(c, 0);
        return;
    }
    std::string profile = vertex ? std::string(result.vertex) : std::string(result.pixel);
    set_eax(c, d3dx_profile_string(profile));
}

void X_D3DXGetVertexShaderProfile(X86 *c) {
    d3dx_shader_profile(c, true);
}

void X_D3DXGetPixelShaderProfile(X86 *c) {
    d3dx_shader_profile(c, false);
}

static bool d3dx_matrix_read(uint32_t addr, float out[16]) {
    if (!addr || !gm_valid(addr, 64))
        return false;
    for (int i = 0; i < 16; ++i)
        out[i] = d3dx_rf(addr + (uint32_t)i * 4);
    return true;
}

static void d3dx_matrix_write(uint32_t addr, const std::array<float, 16> &values) {
    for (size_t i = 0; i < values.size(); ++i)
        d3dx_wf(addr + (uint32_t)i * 4, values[i]);
}

void X_D3DX33MatrixMultiply(X86 *c) {
    uint32_t out = arg(c, 0), a = arg(c, 1), b = arg(c, 2);
    float ma[16], mb[16];
    if (!d3dx_matrix_read(a, ma) || !d3dx_matrix_read(b, mb) || (!out || !gm_valid(out, 64))) {
        set_eax(c, 0);
        return;
    }
    recomp::d3d::D3dxMatrixResult result = recomp::d3d::d3dx_matrix_multiply(
        rust::Slice<const float>(ma, 16), rust::Slice<const float>(mb, 16));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        set_eax(c, 0);
        return;
    }
    d3dx_matrix_write(out, result.values);
    set_eax(c, out);
}

void X_D3DX33MatrixTranspose(X86 *c) {
    uint32_t out = arg(c, 0), in = arg(c, 1);
    float m[16];
    if (!d3dx_matrix_read(in, m) || (!out || !gm_valid(out, 64))) {
        set_eax(c, 0);
        return;
    }
    recomp::d3d::D3dxMatrixResult result =
        recomp::d3d::d3dx_matrix_transpose(rust::Slice<const float>(m, 16));
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        set_eax(c, 0);
        return;
    }
    d3dx_matrix_write(out, result.values);
    set_eax(c, out);
}

void X_D3DX33MatrixInverse(X86 *c) {
    uint32_t out = arg(c, 0), pdet = arg(c, 1), in = arg(c, 2);
    float m[16];
    if (!d3dx_matrix_read(in, m) || (!out || !gm_valid(out, 64)) || (pdet && !gm_valid(pdet, 4))) {
        set_eax(c, 0);
        return;
    }
    recomp::d3d::D3dxMatrixInverseResult result =
        recomp::d3d::d3dx_matrix_inverse(rust::Slice<const float>(m, 16));
    if (result.status.code != recomp::d3d::StatusCode::Ok || !result.valid) {
        set_eax(c, 0);
        return;
    }
    if (pdet)
        d3dx_wf(pdet, result.determinant);
    d3dx_matrix_write(out, result.values);
    set_eax(c, out);
}

static void level_lock_wgpu(X86 *c, ComObj *o, uint32_t face, uint32_t level, bool volume,
                            uint32_t rect_addr, uint32_t box_addr, uint32_t flags, uint32_t out,
                            bool box_output) {
    uint32_t owner_face = 0, owner_level = 0;
    bool owner_volume = false;
    ComObj *owner = level_owner(o, &owner_face, &owner_level, &owner_volume);
    if (owner && owner != o) {
        face = owner_face;
        level = owner_level;
        volume = owner_volume;
    }
    if (!owner || !owner->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    if (!out || !gm_valid(out, box_output ? 12 : 8) || (rect_addr && !gm_valid(rect_addr, 16)) ||
        (box_addr && !gm_valid(box_addr, 24))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9LevelLockRequest request{};
    request.face = face;
    request.level = level;
    request.volume = volume;
    request.full_region = volume ? box_addr == 0 : rect_addr == 0;
    request.flags = flags;
    if (box_addr) {
        request.volume_box.left = rd32(box_addr + 0);
        request.volume_box.top = rd32(box_addr + 4);
        request.volume_box.right = rd32(box_addr + 8);
        request.volume_box.bottom = rd32(box_addr + 12);
        request.volume_box.front = rd32(box_addr + 16);
        request.volume_box.back = rd32(box_addr + 20);
    }
    if (rect_addr) {
        request.rect.left = (int32_t)rd32(rect_addr + 0);
        request.rect.top = (int32_t)rd32(rect_addr + 4);
        request.rect.right = (int32_t)rd32(rect_addr + 8);
        request.rect.bottom = (int32_t)rd32(rect_addr + 12);
    }
    D3d9HostResource *h = owner->d3d9_resource.get();
    if (!d3d9_flush_resource_owner(c, owner))
        return;
    uint64_t key = ((uint64_t)face << 32) | level;
    D3d9Staging &staging = h->staging[key];
    bool fresh = staging.addr == 0;
    recomp::d3d::D3d9LevelLockResult prepared = h->level_lock_prepare(request);
    if (prepared.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, prepared.status));
        return;
    }
    if (fresh) {
        if (prepared.total == 0) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        uint32_t stage = heap_alloc(prepared.total, false, 16);
        if (!stage) {
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
        rust::Slice<const uint8_t> bytes = h->level_bytes(face, level);
        if (bytes.size() != prepared.total) {
            heap_free(stage);
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        memcpy(gm_ptr(stage), bytes.data(), prepared.total);
        staging.addr = stage;
        staging.size = prepared.total;
    }
    recomp::d3d::D3d9LevelLockResult committed = h->level_lock_commit(request);
    if (committed.status.code != recomp::d3d::StatusCode::Ok) {
        if (fresh) {
            heap_free(staging.addr);
            staging.addr = 0;
            staging.size = 0;
        }
        com_ret(c, bridge_result(c, committed.status));
        return;
    }
    if (box_output) {
        wr32(out + 0, prepared.pitch);
        wr32(out + 4, prepared.slice_pitch);
        wr32(out + 8, staging.addr + prepared.offset);
    } else {
        wr32(out + 0, prepared.pitch);
        wr32(out + 4, staging.addr + prepared.offset);
    }
    com_ret(c, D3D_OK9);
}

static void level_unlock_wgpu(X86 *c, ComObj *o, uint32_t face, uint32_t level) {
    uint32_t owner_face = 0, owner_level = 0;
    bool owner_volume = false;
    ComObj *owner = level_owner(o, &owner_face, &owner_level, &owner_volume);
    if (owner && owner != o) {
        face = owner_face;
        level = owner_level;
    }
    if (!owner || !owner->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    D3d9HostResource *h = owner->d3d9_resource.get();
    uint64_t key = ((uint64_t)face << 32) | level;
    auto it = h->staging.find(key);
    if (it == h->staging.end() || !it->second.addr) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const uint8_t> bytes(gm_ptr(it->second.addr), it->second.size);
    if (!d3d9_flush_resource_owner(c, owner))
        return;
    recomp::d3d::D3d9BufferUnlockResult result = h->level_unlock(face, level, bytes);
    if (result.closed) {
        heap_free(it->second.addr);
        h->staging.erase(it);
    }
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_ret(c, D3D_OK9);
}

// (this, Length, Usage, FVF, Pool, ppVertexBuffer, pSharedHandle)
void Dev_CreateVertexBuffer(X86 *c) {
    create_buffer_wgpu(c, recomp::d3d::D3d9BufferKind::Vertex);
}

// (this, Length, Usage, Format, Pool, ppIndexBuffer, pSharedHandle)
void Dev_CreateIndexBuffer(X86 *c) {
    create_buffer_wgpu(c, recomp::d3d::D3d9BufferKind::Index);
}

// (this, Width, Height, Levels, Usage, Format, Pool, ppTexture, pSharedHandle)
void Dev_CreateTexture(X86 *c) {
    create_texture_wgpu(c, recomp::d3d::D3d9TextureKind::Texture2D);
}

// (this, EdgeLength, Levels, Usage, Format, Pool, ppCubeTexture, pSharedHandle)
void Dev_CreateCubeTexture(X86 *c) {
    create_texture_wgpu(c, recomp::d3d::D3d9TextureKind::Cube);
}

void Dev_CreateVolumeTexture(X86 *c) {
    create_texture_wgpu(c, recomp::d3d::D3d9TextureKind::Volume);
}

static void create_surface_method(X86 *c, uint32_t out_index, uint32_t fmt_index, uint32_t usage) {
    create_surface_wgpu(c, out_index, usage);
}
// (this, Width, Height, Format, MultiSample, MultisampleQuality, Discard,
//  ppSurface, pSharedHandle)
void Dev_CreateDepthStencilSurface(X86 *c) {
    create_surface_method(c, 7, 3, USAGE_DEPTHSTENCIL);
}
// (this, Width, Height, Format, MultiSample, MultisampleQuality, Lockable,
//  ppSurface, pSharedHandle)
void Dev_CreateRenderTarget(X86 *c) {
    create_surface_method(c, 7, 3, USAGE_RENDERTARGET);
}
// (this, Width, Height, Format, Pool, ppSurface, pSharedHandle)
void Dev_CreateOffscreenPlainSurface(X86 *c) {
    create_surface_method(c, 5, 3, 0);
}

// The device's own back buffer and depth buffer, made on first ask and kept.
// The back buffer lives in its own field. It used to share render_target,
// which SetRenderTarget overwrites, so after the game switched targets the
// "back buffer" was whatever it had last drawn into.
static ComObj *device_backbuffer(ComObj *dev) {
    if (!dev || !dev->d3d9_device)
        return nullptr;
    ComObj *s = com_get(dev->d3d9_backbuffer);
    if (!s) {
        recomp::d3d::D3d9SurfaceDescriptorResult desc = dev->d3d9_device->backbuffer_descriptor();
        if (desc.status.code != recomp::d3d::StatusCode::Ok)
            return nullptr;
        s = com_new(K_D3D9SURFACE);
        if (!s)
            return nullptr;
        s->d3d9_implicit = true;
        s->d3d9_resource = std::make_shared<D3d9HostResource>();
        s->d3d9_target_depth = false;
        s->dev_d3d = dev->id;
        com_internalize(s);
        dev->d3d9_backbuffer = s->id;
        if (dev->d3d9_device)
            dev->d3d9_device->register_implicit_targets(dev->d3d9_backbuffer,
                                                        dev->d3d9_depthbuffer);
    }
    return s;
}

// (this, pSourceSurface, pSourceRect, pDestSurface, pDestRect, Filter): a
// nearest-neighbour copy between 32-bit surfaces, rectangles honoured. This is
// how a game moves an off-screen target onto the back buffer.
void Dev_StretchRect(X86 *c) {
    ComObj *src = com_this(arg(c, 1)), *dst = com_this(arg(c, 3));
    if (!src || !dst || src->kind != K_D3D9SURFACE || dst->kind != K_D3D9SURFACE) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device || src->dev_d3d != dev->id || dst->dev_d3d != dev->id) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9Rect sr{}, dr{};
    bool have_src = arg(c, 2) != 0;
    bool have_dst = arg(c, 4) != 0;
    if ((have_src && !read_guest_rect(arg(c, 2), &sr)) ||
        (have_dst && !read_guest_rect(arg(c, 4), &dr))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const recomp::d3d::D3d9Rect> srs(have_src ? &sr : nullptr, have_src ? 1 : 0);
    rust::Slice<const recomp::d3d::D3d9Rect> drs(have_dst ? &dr : nullptr, have_dst ? 1 : 0);
    com_ret(c,
            bridge_result(c, dev->d3d9_device->stretch_rect(d3d9_target_id(src), srs,
                                                            d3d9_target_id(dst), drs, arg(c, 5))));
}

void Dev_UpdateSurface(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *src = com_this(arg(c, 1)), *dst = com_this(arg(c, 3));
    if (!dev || !dev->d3d9_device || !src || !dst || src->kind != K_D3D9SURFACE ||
        dst->kind != K_D3D9SURFACE || src->dev_d3d != dev->id || dst->dev_d3d != dev->id ||
        !src->d3d9_resource || !dst->d3d9_resource ||
        src->d3d9_resource.get() == dst->d3d9_resource.get()) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9Rect sr{};
    bool have_src = arg(c, 2) != 0;
    if (have_src && !read_guest_rect(arg(c, 2), &sr)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    int32_t dx = 0, dy = 0;
    if (arg(c, 4)) {
        if (!gm_valid(arg(c, 4), 8)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        dx = (int32_t)rd32(arg(c, 4));
        dy = (int32_t)rd32(arg(c, 4) + 4);
    }
    rust::Slice<const recomp::d3d::D3d9Rect> sr_slice(have_src ? &sr : nullptr, have_src ? 1 : 0);
    com_ret(c, bridge_result(c, dev->d3d9_device->update_surface(
                                    *src->d3d9_resource, src->d3d9_face, src->d3d9_level, sr_slice,
                                    *dst->d3d9_resource, dst->d3d9_face, dst->d3d9_level, dx, dy)));
}

void Dev_UpdateTexture(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *src = com_this(arg(c, 1)), *dst = com_this(arg(c, 2));
    if (!dev || !dev->d3d9_device || !src || !dst || !src->d3d9_resource || !dst->d3d9_resource ||
        src->dev_d3d != dev->id || dst->dev_d3d != dev->id ||
        src->d3d9_resource.get() == dst->d3d9_resource.get()) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(
                   c, dev->d3d9_device->update_texture(*src->d3d9_resource, *dst->d3d9_resource)));
}

void Dev_GetRenderTargetData(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *src = com_this(arg(c, 1)), *dst = com_this(arg(c, 2));
    if (!dev || !dev->d3d9_device || !src || !dst || src->kind != K_D3D9SURFACE ||
        dst->kind != K_D3D9SURFACE || !dst->d3d9_resource || src->dev_d3d != dev->id ||
        dst->dev_d3d != dev->id) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->get_render_target_data(
                                    d3d9_target_id(src), *dst->d3d9_resource, dst->d3d9_face,
                                    dst->d3d9_level)));
}

void Dev_ColorFill(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *s = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || !s || s->kind != K_D3D9SURFACE || s->dev_d3d != dev->id) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9Rect r{};
    bool have = arg(c, 2) != 0;
    if (have && !read_guest_rect(arg(c, 2), &r)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const recomp::d3d::D3d9Rect> slice(have ? &r : nullptr, have ? 1 : 0);
    com_ret(c, bridge_result(c, dev->d3d9_device->color_fill(d3d9_target_id(s), slice, arg(c, 3))));
}
static ComObj *device_depthbuffer(ComObj *dev) {
    if (!dev || !dev->d3d9_device)
        return nullptr;
    ComObj *s = com_get(dev->d3d9_depthbuffer);
    if (!s) {
        recomp::d3d::D3d9SurfaceDescriptorResult desc = dev->d3d9_device->depthbuffer_descriptor();
        if (desc.status.code != recomp::d3d::StatusCode::Ok)
            return nullptr;
        s = com_new(K_D3D9SURFACE);
        if (!s)
            return nullptr;
        s->d3d9_implicit = true;
        s->d3d9_resource = std::make_shared<D3d9HostResource>();
        s->d3d9_target_depth = true;
        s->dev_d3d = dev->id;
        com_internalize(s);
        dev->d3d9_depthbuffer = s->id;
        if (dev->d3d9_device)
            dev->d3d9_device->register_implicit_targets(dev->d3d9_backbuffer,
                                                        dev->d3d9_depthbuffer);
    }
    return s;
}

static bool implicit_surface_externally_referenced(uint32_t field) {
    ComObj *s = com_get(field);
    return s && s->refs > s->internal_refs;
}

static void device_discard_implicit_surfaces(ComObj *dev) {
    for (uint32_t *field : {&dev->d3d9_backbuffer, &dev->d3d9_depthbuffer}) {
        ComObj *s = com_get(*field);
        *field = 0;
        if (s)
            com_release_internal(s);
    }
    dev->render_target = 0;
    if (dev->d3d9_device)
        dev->d3d9_device->register_implicit_targets(0, 0);
}

// (this, iSwapChain, iBackBuffer, Type, ppBackBuffer)
void Dev_GetBackBuffer(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 4);
    if (!dev || arg(c, 1) || arg(c, 2) || arg(c, 3) || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *s = dev ? device_backbuffer(dev) : nullptr;
    if (!s || !out) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_addref(s);
    uint32_t view_ = com_view(s, IF_D3DSURFACE9);
    if (!view_) {
        com_release(s);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view_);
    com_ret(c, D3D_OK9);
}

void Dev_GetDepthStencilSurface(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    recomp::d3d::ValueResult result = dev->d3d9_device->depth_stencil();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (result.value == 0) {
        com_ret(c, D3DERR_NOTFOUND);
        return;
    }
    ComObj *s = result.value == 0xffffffffu ? device_depthbuffer(dev) : com_get(result.value);
    return_surface_handle(c, s, out);
}

// (this, iSwapChain, ppRenderTarget). The surface bound as target 0, or the
// implicit back buffer when none was set; the returned handle is a new
// external reference, like GetBackBuffer's.
void Dev_GetRenderTarget(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || arg(c, 1) != 0 || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    recomp::d3d::ValueResult result = dev->d3d9_device->render_target();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *s = result.value == 0 ? device_backbuffer(dev) : com_get(result.value);
    return_surface_handle(c, s, out);
}

void Dev_SetRenderTarget(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *s = com_this(arg(c, 2));
    uint32_t index = arg(c, 1);
    if (!dev || !dev->d3d9_device || (arg(c, 2) && (!s || s->kind != K_D3D9SURFACE)) ||
        (s && s->dev_d3d != dev->id) || (index == 0 && !s)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t face = 0, level = 0;
    bool volume = false;
    uint32_t id = 0;
    if (s) {
        id = d3d9_target_id(s);
        face = s->d3d9_face;
        level = s->d3d9_level;
        volume = s->d3d9_volume;
    }
    recomp::d3d::Status status =
        dev->d3d9_device->set_render_target(index, id, face, level, volume);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

void Dev_SetDepthStencilSurface(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *s = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || (arg(c, 1) && (!s || s->kind != K_D3D9SURFACE)) ||
        (s && s->dev_d3d != dev->id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t face = 0, level = 0;
    bool volume = false;
    uint32_t id = 0;
    if (s) {
        if (s->d3d9_implicit && s->d3d9_target_depth) {
            id = 0xffffffffu;
        } else {
            id = s->id;
            face = s->d3d9_face;
            level = s->d3d9_level;
            volume = s->d3d9_volume;
        }
    }
    recomp::d3d::Status status = dev->d3d9_device->set_depth_stencil(id, face, level, volume);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

// (this, pVertexElements, ppDecl): the element array is kept verbatim, since
// only the eventual renderer can interpret it.
void Dev_CreateVertexDeclaration(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t elems = arg(c, 1), out = arg(c, 2);
    if (!dev || !out) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<recomp::d3d::D3d9VertexElement> elements;
    for (uint32_t i = 0; i < 65 && elems; ++i) {
        uint32_t e = elems + i * 8u;
        if (!gm_valid(e, 8)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        recomp::d3d::D3d9VertexElement element{};
        element.stream = rd16(e);
        element.offset = rd16(e + 2);
        element.element_type = gm_ptr(e)[4];
        element.method = gm_ptr(e)[5];
        element.usage = gm_ptr(e)[6];
        element.usage_index = gm_ptr(e)[7];
        elements.push_back(element);
        if (element.stream == 0xff)
            break;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->create_vertex_declaration(
        rust::Slice<const recomp::d3d::D3d9VertexElement>(elements.data(), elements.size()));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *d = d3d9_make_object(dev, K_D3D9DECL, IF_D3DVERTEXDECL9, result.value, out);
    if (!d) {
        dev->d3d9_device->destroy_declaration(result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->d3d9_device->decl_views[result.value] = d->id;
    com_ret(c, D3D_OK9);
}

void Dev_SetVertexDeclaration(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *d = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || (arg(c, 1) && (!d || d->kind != K_D3D9DECL)) ||
        (d && d->dev_d3d != dev->id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status =
        dev->d3d9_device->set_vertex_declaration(d ? d->d3d9_state_handle : 0);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

void Dev_CreateQuery(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t type = arg(c, 1), out = arg(c, 2);
    if (!dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    bool known = type == 4 || type == 5 || type == 6 || (type >= 8 && type <= 19);
    if (!known) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->create_query(type);
    if (!out) {
        if (result.status.code == recomp::d3d::StatusCode::Ok && result.value) {
            dev->d3d9_device->destroy_query(result.value);
            com_ret(c, D3D_OK9);
        } else {
            com_ret(c, D3DERR_NOTAVAILABLE);
        }
        return;
    }
    if (!gm_valid(out, 4)) {
        if (result.status.code == recomp::d3d::StatusCode::Ok && result.value)
            dev->d3d9_device->destroy_query(result.value);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *q = com_new(K_D3D9QUERY);
    if (!q) {
        dev->d3d9_device->destroy_query(result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    q->dev_d3d = dev->id;
    q->d3d9_query = result.value;
    com_retain_internal(dev);
    q->d3d9_owner_retained = true;
    uint32_t view_ = com_view(q, IF_D3DQUERY9);
    if (!view_) {
        com_release(q);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view_);
    com_ret(c, D3D_OK9);
}

// (this, StreamNumber, pStreamData, OffsetInBytes, Stride)
void Dev_SetStreamSource(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *vb = com_this(arg(c, 2));
    uint32_t stream = arg(c, 1);
    if (!dev || !dev->d3d9_device || stream >= 16 || (arg(c, 2) && (!vb || vb->kind != K_D3D9VB)) ||
        (vb && vb->dev_d3d != dev->id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status =
        dev->d3d9_device->set_stream_source(stream, vb ? vb->id : 0, arg(c, 3), arg(c, 4));
    if (d3d_bridge_ok(status))
        d3d9_bind_slot(dev, 16 + stream, vb ? vb->id : 0);
    com_ret(c, bridge_result(c, status));
}

void Dev_SetIndices(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *ib = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || (arg(c, 1) && (!ib || ib->kind != K_D3D9IB)) ||
        (ib && ib->dev_d3d != dev->id)) {
        log_once(("d3d9.SetIndices:" + std::to_string(arg(c, 1))).c_str(),
                 "d3d9: SetIndices rejected device=%08x id=%08x buffer=%08x id=%08x kind=%u "
                 "owner=%08x",
                 arg(c, 0), dev ? dev->id : 0, arg(c, 1), ib ? ib->id : 0,
                 ib ? static_cast<unsigned>(ib->kind) : 0, ib ? ib->dev_d3d : 0);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->set_indices(ib ? ib->id : 0);
    if (d3d_bridge_ok(status))
        d3d9_bind_slot(dev, 32, ib ? ib->id : 0);
    com_ret(c, bridge_result(c, status));
}

// (this, Stage, pTexture): the texture behind a sampler, for the renderer.
void Dev_SetTexture(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *tex = com_this(arg(c, 2));
    uint32_t stage = arg(c, 1);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (stage >= 16) {
        if (stage >= D3DDMAPSAMPLER && stage <= D3DVERTEXTEXTURESAMPLER3)
            imports_unsupported(c);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (arg(c, 2)) {
        if (!tex || tex->dev_d3d != dev->id ||
            (tex->kind != K_D3D9TEXTURE && tex->kind != K_D3D9CUBETEXTURE &&
             tex->kind != K_D3D9VOLUMETEXTURE)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
    }
    recomp::d3d::Status status = dev->d3d9_device->set_texture(stage, tex ? tex->id : 0);
    if (d3d_bridge_ok(status))
        d3d9_bind_slot(dev, stage, tex ? tex->id : 0);
    com_ret(c, bridge_result(c, status));
}

static uint32_t g_draws = 0;

static const char *prim_name(uint32_t type) {
    switch (type) {
    case 1:
        return "point list";
    case 2:
        return "line list";
    case 3:
        return "line strip";
    case 4:
        return "triangle list";
    case 5:
        return "triangle strip";
    case 6:
        return "triangle fan";
    default:
        return "unknown primitive";
    }
}

// The first few draws, then (with verbose logging) every 256th: a race makes
// thousands a frame, and writing even one in 256 cost a percent of the frame.
static bool draw_worth_reporting() {
    ++g_draws;
    return g_draws <= 8 || ((g_draws % 256) == 0 && log_level() >= 2);
}

static uint64_t primitive_vertices(uint32_t prim, uint32_t count) {
    uint64_t n = count;
    switch (prim) {
    case 1:
        return n;
    case 2:
        return n * 2;
    case 3:
        return n + 1;
    case 4:
        return n * 3;
    case 5:
    case 6:
        return n + 2;
    default:
        return 0;
    }
}

static uint32_t inline_bytes(uint64_t count, uint32_t stride) {
    if (!stride || count > UINT32_MAX / stride)
        return 0;
    return static_cast<uint32_t>(count) * stride;
}

// (this, PrimitiveType, StartVertex, PrimitiveCount)
void Dev_DrawPrimitive(X86 *c) {
    if (draw_worth_reporting())
        LOGW("d3d9: draw %u: %s, %u primitives from vertex %u, from the bound stream", g_draws,
             prim_name(arg(c, 1)), arg(c, 3), arg(c, 2));
    ComObj *dev = this_device9(c);
    uint32_t prim_count = arg(c, 3);
    if (prim_count == 0) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!d3d9_sync_locked_buffers(c, dev))
        return;
    recomp::d3d::Status status = dev->d3d9_device->draw_primitive(arg(c, 1), arg(c, 2), prim_count);
    if (status.code == recomp::d3d::StatusCode::Ok)
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

// (this, Type, BaseVertexIndex, MinVertexIndex, NumVertices, startIndex, primCount)
void Dev_DrawIndexedPrimitive(X86 *c) {
    if (draw_worth_reporting())
        LOGW("d3d9: draw %u: %s, %u primitives, %u vertices from index %u, indexed", g_draws,
             prim_name(arg(c, 1)), arg(c, 6), arg(c, 4), arg(c, 5));
    ComObj *dev = this_device9(c);
    uint32_t prim_count = arg(c, 6);
    if (prim_count == 0) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!d3d9_sync_locked_buffers(c, dev))
        return;
    recomp::d3d::Status status = dev->d3d9_device->draw_indexed_primitive(
        arg(c, 1), (int32_t)arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), prim_count);
    com_ret(c, bridge_result(c, status));
}

// (this, PrimitiveType, PrimitiveCount, pVertexStreamZeroData, Stride): the
// vertices travel with the call rather than in a buffer, which is how this
// game draws.

void Dev_DrawPrimitiveUP(X86 *c) {
    if (draw_worth_reporting()) {
        LOGW("d3d9: draw %u: %s, %u primitives, %u-byte vertices inline at %08x", g_draws,
             prim_name(arg(c, 1)), arg(c, 2), arg(c, 4), arg(c, 3));
    }
    ComObj *dev = this_device9(c);
    uint32_t prim_count = arg(c, 2);
    if (prim_count == 0) {
        com_ret(c, D3D_OK9);
        return;
    }
    uint32_t bytes = inline_bytes(primitive_vertices(arg(c, 1), prim_count), arg(c, 4));
    if (!dev || !bytes || !arg(c, 3) || !gm_valid(arg(c, 3), bytes)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->draw_primitive_up(
        arg(c, 1), prim_count, rust::Slice<const uint8_t>(gm_ptr(arg(c, 3)), bytes), arg(c, 4));
    d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

// (this, PrimitiveType, MinVertexIndex, NumVertices, PrimitiveCount,
//  pIndexData, IndexDataFormat, pVertexStreamZeroData, Stride)
void Dev_DrawIndexedPrimitiveUP(X86 *c) {
    if (draw_worth_reporting())
        LOGW("d3d9: draw %u: %s, %u primitives, %u vertices of %u bytes inline at %08x, "
             "indices at %08x",
             g_draws, prim_name(arg(c, 1)), arg(c, 4), arg(c, 3), arg(c, 8), arg(c, 7), arg(c, 5));
    ComObj *dev = this_device9(c);
    uint32_t prim_count = arg(c, 4);
    if (prim_count == 0) {
        com_ret(c, D3D_OK9);
        return;
    }
    uint32_t isize = arg(c, 6) == D3DFMT_INDEX32 ? 4 : 2;
    uint32_t ibytes = inline_bytes(primitive_vertices(arg(c, 1), prim_count), isize);
    uint32_t vbytes = inline_bytes(static_cast<uint64_t>(arg(c, 2)) + arg(c, 3), arg(c, 8));
    if (!dev || (arg(c, 6) != D3DFMT_INDEX16 && arg(c, 6) != D3DFMT_INDEX32) || !ibytes ||
        !vbytes || !arg(c, 5) || !arg(c, 7) || !gm_valid(arg(c, 5), ibytes) ||
        !gm_valid(arg(c, 7), vbytes)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->draw_indexed_primitive_up(
        arg(c, 1), arg(c, 2), arg(c, 3), prim_count,
        rust::Slice<const uint8_t>(gm_ptr(arg(c, 5)), ibytes), arg(c, 6),
        rust::Slice<const uint8_t>(gm_ptr(arg(c, 7)), vbytes), arg(c, 8));
    d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

// ---------------------------------------------------------------------------
// The resource interfaces themselves
// ---------------------------------------------------------------------------

static void resource_type_wgpu(X86 *c, ComObj *o) {
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = o->d3d9_resource->resource_type();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_ret(c, result.value);
}

void Res_GetType(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (o && o->d3d9_implicit) {
        com_ret(c, 1);
        return;
    }
    resource_type_wgpu(c, o);
}

void Buf_GetType(X86 *c) {
    Res_GetType(c);
}

static const uint8_t kIidUnknown9[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46};

static void resource_container(X86 *c) {
    ComObj *s = com_this_arg(c);
    uint32_t riid = arg(c, 1), out = arg(c, 2);
    if (!s || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_out_ptr(out, 0);
    ComObj *target = s->d3d9_parent ? com_get(s->d3d9_parent) : com_get(s->dev_d3d);
    if (!target) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!riid || !gm_valid(riid, 16)) {
        com_ret(c, E_POINTER);
        return;
    }
    ComIface want = com_iface_for_iid(riid);
    if (want == IF_NONE) {
        if (memcmp(gm_ptr(riid), kIidUnknown9, 16) != 0) {
            com_ret(c, E_NOINTERFACE);
            return;
        }
        if (!target->identity) {
            com_ret(c, E_NOINTERFACE);
            return;
        }
        com_addref(target);
        com_out_ptr(out, target->identity);
        com_ret(c, D3D_OK9);
        return;
    }
    if (!com_iface_binds(want, target->kind)) {
        com_ret(c, E_NOINTERFACE);
        return;
    }
    uint32_t view = com_view(target, want);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(target);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

// (this, riid, ppContainer): the texture a level or face belongs to, or the
// device when the surface stands alone.
void Surf_GetContainer(X86 *c) {
    resource_container(c);
}

void Res_GetDevice(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t out = arg(c, 1);
    if (!o || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!o->d3d9_resource && !o->d3d9_implicit && !o->dev_d3d) {
        imports_unsupported(c);
        return;
    }
    ComObj *dev = com_get(o->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(dev, IF_D3DDEVICE9);
    if (!view) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_addref(dev);
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void Buf_GetDevice(X86 *c) {
    Res_GetDevice(c);
}

static uint32_t private_view_key(ComObj *o) {
    if (!o || !o->d3d9_parent)
        return 0;
    uint32_t key = 0x80000000u | ((o->d3d9_face & 0x1fu) << 8) | (o->d3d9_level & 0xffu);
    if (o->d3d9_volume)
        key |= 0x40000000u;
    return key;
}

static void resource_set_private_data(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t guid = arg(c, 1), data = arg(c, 2), size = arg(c, 3), flags = arg(c, 4);
    if (!o || !o->d3d9_resource || !guid || !gm_valid(guid, 16) ||
        (size && (!data || !gm_valid(data, size))) || (flags & ~1u)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (flags) {
        LOGW("d3d9: IUnknown private data is not implemented");
        imports_unsupported(c);
        return;
    }
    rust::Slice<const uint8_t> g(gm_ptr(guid), 16);
    rust::Slice<const uint8_t> d(data ? gm_ptr(data) : nullptr, size);
    recomp::d3d::Status status =
        o->d3d9_resource->set_private_data(private_view_key(o), g, d, flags);
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

static void resource_get_private_data(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t guid = arg(c, 1), data = arg(c, 2), size_addr = arg(c, 3);
    if (!o || !o->d3d9_resource || !guid || !gm_valid(guid, 16) || !size_addr ||
        !gm_valid(size_addr, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t have = rd32(size_addr);
    if (data && have && !gm_valid(data, have)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const uint8_t> g(gm_ptr(guid), 16);
    recomp::d3d::D3d9PrivateDataResult result =
        o->d3d9_resource->get_private_data(private_view_key(o), g);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!result.found) {
        com_ret(c, D3DERR_NOTFOUND);
        return;
    }
    uint32_t need = (uint32_t)result.data.size();
    if (!data) {
        wr32(size_addr, need);
        com_ret(c, D3D_OK9);
        return;
    }
    if (have < need) {
        wr32(size_addr, need);
        com_ret(c, D3DERR_MOREDATA);
        return;
    }
    if (need) {
        if (!gm_valid(data, need)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        memcpy(gm_ptr(data), result.data.data(), need);
    }
    wr32(size_addr, need);
    com_ret(c, D3D_OK9);
}

static void resource_free_private_data(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t guid = arg(c, 1);
    if (!o || !o->d3d9_resource || !guid || !gm_valid(guid, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    rust::Slice<const uint8_t> g(gm_ptr(guid), 16);
    recomp::d3d::Status status = o->d3d9_resource->free_private_data(private_view_key(o), g);
    if (status.code == recomp::d3d::StatusCode::Ok)
        com_ret(c, D3D_OK9);
    else if (status.code == recomp::d3d::StatusCode::InvalidArgument)
        com_ret(c, D3DERR_NOTFOUND);
    else
        com_ret(c, bridge_result(c, status));
}

static void resource_set_priority(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = o->d3d9_resource->set_priority(arg(c, 1));
    com_ret(c, result.status.code == recomp::d3d::StatusCode::Ok ? result.value
                                                                 : bridge_result(c, result.status));
}

static void resource_get_priority(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = o->d3d9_resource->priority();
    com_ret(c, result.status.code == recomp::d3d::StatusCode::Ok ? result.value
                                                                 : bridge_result(c, result.status));
}

static void resource_set_lod(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = o->d3d9_resource->set_lod(arg(c, 1));
    com_ret(c, result.status.code == recomp::d3d::StatusCode::Ok ? result.value
                                                                 : bridge_result(c, result.status));
}

static void resource_get_lod(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = o->d3d9_resource->lod();
    com_ret(c, result.status.code == recomp::d3d::StatusCode::Ok ? result.value
                                                                 : bridge_result(c, result.status));
}

static void resource_preload(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    if (!d3d9_flush_resource_owner(c, o))
        return;
    recomp::d3d::Status status = o->d3d9_resource->preload();
    if (status.code != recomp::d3d::StatusCode::Ok) {
        std::string message(status.message.data(), status.message.size());
        LOGW("d3d9: PreLoad: %s", message.c_str());
        com_ret(c, bridge_result(c, status));
        return;
    }
    com_ret(c, D3D_OK9);
}

static void buffer_lock_wgpu(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t offset = arg(c, 1), size = arg(c, 2), out = arg(c, 3), flags = arg(c, 4);
    if (!o || (o->kind != K_D3D9VB && o->kind != K_D3D9IB) || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9BufferLockRequest request{};
    request.offset = offset;
    request.size = size;
    request.flags = flags;
    recomp::d3d::D3d9BufferLockResult prepared = o->d3d9_resource->lock_prepare(request);
    if (prepared.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, prepared.status));
        return;
    }
    D3d9Staging &staging = o->d3d9_resource->staging[0];
    bool fresh = staging.addr == 0;
    if (fresh) {
        if (prepared.total == 0) {
            d3d9_free_staging(staging);
            o->d3d9_resource->staging.erase(0);
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        uint32_t stage = heap_alloc(prepared.total, false, 16);
        if (!stage) {
            d3d9_free_staging(staging);
            o->d3d9_resource->staging.erase(0);
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
        staging.addr = stage;
        staging.size = prepared.total;
        staging.retain =
            prepared.total <= D3D9_RETAINED_STAGING_LIMIT - g_d3d9_retained_staging_bytes;
        if (staging.retain)
            g_d3d9_retained_staging_bytes += prepared.total;
    }
    bool starting = !staging.active;
    if (starting && !staging.synchronized) {
        rust::Slice<const uint8_t> bytes = o->d3d9_resource->bytes();
        if (bytes.size() != prepared.total || staging.size != prepared.total) {
            if (fresh) {
                d3d9_free_staging(staging);
                o->d3d9_resource->staging.erase(0);
            }
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        memcpy(gm_ptr(staging.addr), bytes.data(), prepared.total);
    }
    recomp::d3d::D3d9BufferLockResult committed = o->d3d9_resource->lock_commit(request);
    if (committed.status.code != recomp::d3d::StatusCode::Ok) {
        if (fresh) {
            d3d9_free_staging(staging);
            o->d3d9_resource->staging.erase(0);
        }
        com_ret(c, bridge_result(c, committed.status));
        return;
    }
    if (starting) {
        staging.active = true;
        staging.synchronized = false;
        staging.writable = false;
    }
    if ((flags & D3D9_LOCK_READONLY) == 0)
        staging.writable = true;
    wr32(out, staging.addr + committed.offset);
    com_ret(c, D3D_OK9);
}

static void buffer_unlock_wgpu(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (!o || (o->kind != K_D3D9VB && o->kind != K_D3D9IB) || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    auto it = o->d3d9_resource->staging.find(0);
    if (it == o->d3d9_resource->staging.end() || !it->second.addr) {
        com_ret(c, D3D_OK9);
        return;
    }
    rust::Slice<const uint8_t> bytes(gm_ptr(it->second.addr), it->second.size);
    recomp::d3d::D3d9BufferUnlockResult result = o->d3d9_resource->unlock(bytes);
    if (result.closed) {
        it->second.active = false;
        it->second.synchronized =
            result.status.code == recomp::d3d::StatusCode::Ok && it->second.writable;
        it->second.writable = false;
        if (!it->second.retain || result.status.code != recomp::d3d::StatusCode::Ok) {
            d3d9_free_staging(it->second);
            o->d3d9_resource->staging.erase(it);
        }
    }
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_ret(c, D3D_OK9);
}

void VB_Lock(X86 *c) {
    buffer_lock_wgpu(c);
}
void IB_Lock(X86 *c) {
    buffer_lock_wgpu(c);
}
void Buf_Unlock(X86 *c) {
    buffer_unlock_wgpu(c);
}
// D3DVERTEXBUFFER_DESC / D3DINDEXBUFFER_DESC: Format, Type, Usage, Pool, Size.
void Buf_GetDesc(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t d = arg(c, 1);
    if (!o || (o->kind != K_D3D9VB && o->kind != K_D3D9IB) || !o->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    uint32_t desc_size = o->kind == K_D3D9VB ? 24 : 20;
    if (!d || !gm_valid(d, desc_size)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9BufferDescriptorResult result = o->d3d9_resource->descriptor();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    memset(gm_ptr(d), 0, desc_size);
    wr32(d + 0, result.descriptor.format);
    wr32(d + 4, result.descriptor.resource_type);
    wr32(d + 8, result.descriptor.usage);
    wr32(d + 12, result.descriptor.pool);
    wr32(d + 16, result.descriptor.size);
    if (o->kind == K_D3D9VB)
        wr32(d + 20, result.descriptor.fvf);
    com_ret(c, D3D_OK9);
}

// D3DLOCKED_RECT is { INT Pitch; void *pBits; }.
void Surf_LockRect(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (o && o->d3d9_implicit) {
        imports_unsupported(c);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    level_lock_wgpu(c, o, 0, 0, false, arg(c, 2), 0, arg(c, 3), arg(c, 1), false);
}
void Surf_UnlockRect(X86 *c) {
    ComObj *o = com_this_arg(c);
    if (o && o->d3d9_implicit) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    level_unlock_wgpu(c, o, 0, 0);
}
// D3DSURFACE_DESC: Format, Type, Usage, Pool, MultiSampleType, Quality,
// Width, Height.
void Surf_GetDesc(X86 *c) {
    ComObj *s = com_this_arg(c);
    uint32_t d = arg(c, 1);
    if (s && s->d3d9_implicit) {
        ComObj *dev = com_get(s->dev_d3d);
        if (!dev || !dev->d3d9_device || !d || !gm_valid(d, 32)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        recomp::d3d::D3d9SurfaceDescriptorResult result =
            s->d3d9_target_depth ? dev->d3d9_device->depthbuffer_descriptor()
                                 : dev->d3d9_device->backbuffer_descriptor();
        if (result.status.code != recomp::d3d::StatusCode::Ok) {
            com_ret(c, bridge_result(c, result.status));
            return;
        }
        memset(gm_ptr(d), 0, 32);
        wr32(d + 0, result.descriptor.format);
        wr32(d + 4, result.descriptor.resource_type);
        wr32(d + 8, result.descriptor.usage);
        wr32(d + 12, result.descriptor.pool);
        wr32(d + 24, result.descriptor.width);
        wr32(d + 28, result.descriptor.height);
        com_ret(c, D3D_OK9);
        return;
    }
    if (!s || !s->d3d9_resource || !d || !gm_valid(d, 32)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (s->d3d9_parent) {
        ComObj *parent = com_get(s->d3d9_parent);
        if (!parent || !parent->d3d9_resource) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
        recomp::d3d::D3d9TextureDescriptorResult td = parent->d3d9_resource->texture_descriptor();
        recomp::d3d::D3d9LevelDescriptorResult ld =
            parent->d3d9_resource->texture_level_descriptor(s->d3d9_face, s->d3d9_level);
        if (td.status.code != recomp::d3d::StatusCode::Ok) {
            com_ret(c, bridge_result(c, td.status));
            return;
        }
        if (ld.status.code != recomp::d3d::StatusCode::Ok) {
            com_ret(c, bridge_result(c, ld.status));
            return;
        }
        memset(gm_ptr(d), 0, 32);
        wr32(d + 0, ld.descriptor.format);
        wr32(d + 4, td.descriptor.resource_type);
        wr32(d + 8, td.descriptor.usage);
        wr32(d + 12, td.descriptor.pool);
        wr32(d + 24, ld.descriptor.width);
        wr32(d + 28, ld.descriptor.height);
        com_ret(c, D3D_OK9);
        return;
    }
    recomp::d3d::D3d9SurfaceDescriptorResult result = s->d3d9_resource->surface_descriptor();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    memset(gm_ptr(d), 0, 32);
    wr32(d + 0, result.descriptor.format);
    wr32(d + 4, result.descriptor.resource_type);
    wr32(d + 8, result.descriptor.usage);
    wr32(d + 12, result.descriptor.pool);
    wr32(d + 16, result.descriptor.multisample_type);
    wr32(d + 20, result.descriptor.multisample_quality);
    wr32(d + 24, result.descriptor.width);
    wr32(d + 28, result.descriptor.height);
    com_ret(c, D3D_OK9);
}

// (this, FaceType, Level, ppCubeMapSurface). A face is a view into the
// texture, so handing one out references the texture too: the game takes
// face zero, releases the cube, and goes on using it for the other five.
void Tex_GetCubeMapSurface(X86 *c) {
    ComObj *tex = com_this_arg(c);
    uint32_t out = arg(c, 3);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *s = com_level_view(tex, arg(c, 1), arg(c, 2), false);
    if (!s) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(s, IF_D3DSURFACE9);
    if (!view) {
        com_release(s);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

// Returns a count, not an HRESULT. The stub returned zero, which told the
// game its textures had no levels at all.
// (this, FaceType, Level, pLockedRect, pRect, Flags)
void Tex_LockRectCube(X86 *c) {
    level_lock_wgpu(c, com_this_arg(c), arg(c, 1), arg(c, 2), false, arg(c, 4), 0, arg(c, 5),
                    arg(c, 3), false);
}
// (this, FaceType, Level)
void Tex_UnlockRectCube(X86 *c) {
    level_unlock_wgpu(c, com_this_arg(c), arg(c, 1), arg(c, 2));
}

void Tex_GetLevelCount(X86 *c) {
    ComObj *tex = com_this_arg(c);
    if (!tex || !tex->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult result = tex->d3d9_resource->texture_descriptor();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_ret(c, result.descriptor.levels);
}

// (this, Level, pDesc): the same D3DSURFACE_DESC a surface describes itself
// with, for the level the texture was created at.
void Tex_GetLevelDesc(X86 *c) {
    ComObj *tex = com_this_arg(c);
    uint32_t d = arg(c, 2);
    if (!tex || !tex->d3d9_resource || !d || !gm_valid(d, 32)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult td = tex->d3d9_resource->texture_descriptor();
    recomp::d3d::D3d9LevelDescriptorResult ld =
        tex->d3d9_resource->texture_level_descriptor(0, arg(c, 1));
    if (td.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, td.status));
        return;
    }
    if (ld.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, ld.status));
        return;
    }
    memset(gm_ptr(d), 0, 32);
    wr32(d + 0, ld.descriptor.format);
    wr32(d + 4, td.descriptor.resource_type);
    wr32(d + 8, td.descriptor.usage);
    wr32(d + 12, td.descriptor.pool);
    wr32(d + 24, ld.descriptor.width);
    wr32(d + 28, ld.descriptor.height);
    com_ret(c, D3D_OK9);
}

void Tex_GetSurfaceLevel(X86 *c) {
    ComObj *tex = com_this_arg(c);
    uint32_t out = arg(c, 2);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *s = com_level_view(tex, 0, arg(c, 1), false);
    if (!s) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(s, IF_D3DSURFACE9);
    if (!view) {
        com_release(s);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}
void Tex_LockRect(X86 *c) {
    level_lock_wgpu(c, com_this_arg(c), 0, arg(c, 1), false, arg(c, 3), 0, arg(c, 4), arg(c, 2),
                    false);
}
void Tex_UnlockRect(X86 *c) {
    level_unlock_wgpu(c, com_this_arg(c), 0, arg(c, 1));
}

static void autogen_set_filter(X86 *c) {
    ComObj *tex = com_this_arg(c);
    if (!tex || !tex->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::Status status = tex->d3d9_resource->set_autogen_filter(arg(c, 1));
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

static void autogen_get_filter(X86 *c) {
    ComObj *tex = com_this_arg(c);
    if (!tex || !tex->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = tex->d3d9_resource->autogen_filter();
    com_ret(c, result.status.code == recomp::d3d::StatusCode::Ok ? result.value
                                                                 : bridge_result(c, result.status));
}

static void autogen_generate(X86 *c) {
    ComObj *tex = com_this_arg(c);
    if (!tex || !tex->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    if (!d3d9_flush_resource_owner(c, tex))
        return;
    recomp::d3d::Status status = tex->d3d9_resource->generate_mips();
    if (status.code != recomp::d3d::StatusCode::Ok) {
        std::string message(status.message.data(), status.message.size());
        LOGW("d3d9: GenerateMipSubLevels: %s", message.c_str());
    }
}

static bool read_dirty_rect(uint32_t rect, recomp::d3d::D3d9LockRect *out) {
    if (!rect)
        return true;
    if (!gm_valid(rect, 16))
        return false;
    out->left = (int32_t)rd32(rect + 0);
    out->top = (int32_t)rd32(rect + 4);
    out->right = (int32_t)rd32(rect + 8);
    out->bottom = (int32_t)rd32(rect + 12);
    return true;
}

static bool read_dirty_box(uint32_t box, recomp::d3d::D3d9LockBox *out) {
    if (!box)
        return true;
    if (!gm_valid(box, 24))
        return false;
    out->left = rd32(box + 0);
    out->top = rd32(box + 4);
    out->right = rd32(box + 8);
    out->bottom = rd32(box + 12);
    out->front = rd32(box + 16);
    out->back = rd32(box + 20);
    return true;
}

void Tex_AddDirtyRect(X86 *c) {
    ComObj *tex = com_this_arg(c);
    recomp::d3d::D3d9LockRect rect{};
    if (!tex || !tex->d3d9_resource || !read_dirty_rect(arg(c, 1), &rect)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = tex->d3d9_resource->add_dirty_rect(0, 0, arg(c, 1) == 0, rect);
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

void Tex_AddDirtyRectCube(X86 *c) {
    ComObj *tex = com_this_arg(c);
    recomp::d3d::D3d9LockRect rect{};
    if (!tex || !tex->d3d9_resource || !read_dirty_rect(arg(c, 2), &rect)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status =
        tex->d3d9_resource->add_dirty_rect(arg(c, 1), 0, arg(c, 2) == 0, rect);
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

void VolTex_GetLevelCount(X86 *c) {
    ComObj *tex = com_this_arg(c);
    if (!tex || !tex->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult result = tex->d3d9_resource->texture_descriptor();
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    com_ret(c, result.descriptor.levels);
}

void VolTex_GetLevelDesc(X86 *c) {
    ComObj *tex = com_this_arg(c);
    uint32_t d = arg(c, 2);
    if (!tex || !tex->d3d9_resource || !d || !gm_valid(d, 28)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult td = tex->d3d9_resource->texture_descriptor();
    recomp::d3d::D3d9LevelDescriptorResult ld =
        tex->d3d9_resource->texture_volume_descriptor(arg(c, 1));
    if (td.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, td.status));
        return;
    }
    if (ld.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, ld.status));
        return;
    }
    memset(gm_ptr(d), 0, 28);
    wr32(d + 0, ld.descriptor.format);
    wr32(d + 4, td.descriptor.resource_type);
    wr32(d + 8, td.descriptor.usage);
    wr32(d + 12, td.descriptor.pool);
    wr32(d + 16, ld.descriptor.width);
    wr32(d + 20, ld.descriptor.height);
    wr32(d + 24, ld.descriptor.depth);
    com_ret(c, D3D_OK9);
}

void VolTex_GetVolumeLevel(X86 *c) {
    ComObj *tex = com_this_arg(c);
    uint32_t out = arg(c, 2);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *v = com_level_view(tex, 0, arg(c, 1), true);
    if (!v) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t view = com_view(v, IF_D3DVOLUME9);
    if (!view) {
        com_release(v);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, D3D_OK9);
}

void VolTex_LockBox(X86 *c) {
    level_lock_wgpu(c, com_this_arg(c), 0, arg(c, 1), true, 0, arg(c, 3), arg(c, 4), arg(c, 2),
                    true);
}

void VolTex_UnlockBox(X86 *c) {
    level_unlock_wgpu(c, com_this_arg(c), 0, arg(c, 1));
}

void VolTex_AddDirtyBox(X86 *c) {
    ComObj *tex = com_this_arg(c);
    recomp::d3d::D3d9LockBox box{};
    if (!tex || !tex->d3d9_resource || !read_dirty_box(arg(c, 1), &box)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = tex->d3d9_resource->add_dirty_box(0, arg(c, 1) == 0, box);
    com_ret(c, status.code == recomp::d3d::StatusCode::Ok ? D3D_OK9 : bridge_result(c, status));
}

void Vol_GetContainer(X86 *c) {
    resource_container(c);
}

void Vol_GetDesc(X86 *c) {
    ComObj *v = com_this_arg(c);
    uint32_t d = arg(c, 1);
    if (!v || !v->d3d9_resource || !d || !gm_valid(d, 28)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t face = 0, level = 0;
    bool volume = false;
    ComObj *owner = level_owner(v, &face, &level, &volume);
    if (!owner || !owner->d3d9_resource) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::D3d9TextureDescriptorResult td = owner->d3d9_resource->texture_descriptor();
    recomp::d3d::D3d9LevelDescriptorResult ld =
        owner->d3d9_resource->texture_volume_descriptor(level);
    if (td.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, td.status));
        return;
    }
    if (ld.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, ld.status));
        return;
    }
    memset(gm_ptr(d), 0, 28);
    wr32(d + 0, ld.descriptor.format);
    wr32(d + 4, td.descriptor.resource_type);
    wr32(d + 8, td.descriptor.usage);
    wr32(d + 12, td.descriptor.pool);
    wr32(d + 16, ld.descriptor.width);
    wr32(d + 20, ld.descriptor.height);
    wr32(d + 24, ld.descriptor.depth);
    com_ret(c, D3D_OK9);
}

void Vol_LockBox(X86 *c) {
    level_lock_wgpu(c, com_this_arg(c), 0, 0, true, 0, arg(c, 2), arg(c, 3), arg(c, 1), true);
}

void Vol_UnlockBox(X86 *c) {
    level_unlock_wgpu(c, com_this_arg(c), 0, 0);
}

static const uint32_t S_FALSE9 = 1;
static void query_destroy(ComObj *o) {
    if (o->d3d9_query) {
        if (ComObj *dev = com_get(o->dev_d3d)) {
            if (dev->d3d9_device)
                dev->d3d9_device->destroy_query(o->d3d9_query);
        }
        o->d3d9_query = 0;
    }
    if (o->d3d9_owner_retained) {
        o->d3d9_owner_retained = false;
        if (ComObj *dev = com_get(o->dev_d3d))
            com_release_internal(dev);
    }
    o->dev_d3d = 0;
}
void Query_GetType9(X86 *c) {
    ComObj *q = com_this_arg(c);
    ComObj *dev = q ? com_get(q->dev_d3d) : nullptr;
    if (!q || !q->d3d9_query || !dev || !dev->d3d9_device) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->query_type(q->d3d9_query);
    if (!d3d_bridge_ok(result.status)) {
        bridge_result(c, result.status);
        return;
    }
    set_eax(c, result.value);
}
void Query_GetDataSize(X86 *c) {
    ComObj *q = com_this_arg(c);
    ComObj *dev = q ? com_get(q->dev_d3d) : nullptr;
    if (!q || !q->d3d9_query || !dev || !dev->d3d9_device) {
        imports_unsupported(c);
        return;
    }
    recomp::d3d::ValueResult kind = dev->d3d9_device->query_type(q->d3d9_query);
    if (!d3d_bridge_ok(kind.status)) {
        bridge_result(c, kind.status);
        return;
    }
    recomp::d3d::ValueResult result = recomp::d3d::d3d9_query_data_size(kind.value);
    if (!d3d_bridge_ok(result.status)) {
        bridge_result(c, result.status);
        return;
    }
    set_eax(c, result.value);
}
// (this, dwIssueFlags): D3DISSUE_END 1, D3DISSUE_BEGIN 2.
void Query_Issue(X86 *c) {
    ComObj *q = com_this_arg(c);
    uint32_t flags = arg(c, 1);
    if (!q || (flags != D3DISSUE_BEGIN && flags != D3DISSUE_END)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!q->d3d9_query) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(q->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->query_issue(q->d3d9_query, flags)));
}
// (this, pData, dwSize, dwGetDataFlags)
void Query_GetData(X86 *c) {
    ComObj *q = com_this_arg(c);
    uint32_t data = arg(c, 1), size = arg(c, 2), flags = arg(c, 3);
    if (!q || (size != 0 && (size != 4 || !data || !gm_valid(data, size))) ||
        (flags & ~D3DGETDATA_FLUSH)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!q->d3d9_query) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (size) {
        if (!data || !gm_valid(data, size)) {
            com_ret(c, D3DERR_INVALIDCALL);
            return;
        }
    } else if (size != 0) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(q->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint8_t staged[4]{};
    rust::Slice<uint8_t> out(size ? staged : nullptr, size);
    recomp::d3d::ValueResult result =
        dev->d3d9_device->query_get_data(q->d3d9_query, out, size, flags);
    if (result.status.code != recomp::d3d::StatusCode::Ok) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!result.value) {
        com_ret(c, S_FALSE9);
        return;
    }
    if (size)
        memcpy(gm_ptr(data), staged, size);
    com_ret(c, D3D_OK9);
}

// (this, State, Value)
// (this, pViewport): D3DVIEWPORT9 is X, Y, Width, Height, MinZ, MaxZ.
void Dev_SetViewport(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t v = arg(c, 1);
    if (!dev || !dev->d3d9_device || !v || !gm_valid(v, 24)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Viewport viewport{};
    viewport.x = rd32(v);
    viewport.y = rd32(v + 4);
    viewport.width = rd32(v + 8);
    viewport.height = rd32(v + 12);
    memcpy(&viewport.min_z, gm_ptr(v + 16), 4);
    memcpy(&viewport.max_z, gm_ptr(v + 20), 4);
    com_ret(c, bridge_result(c, dev->d3d9_device->set_viewport(viewport)));
}
void Dev_GetViewport(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t v = arg(c, 1);
    if (!dev || !dev->d3d9_device || !v || !gm_valid(v, 24)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ViewportResult result = dev->d3d9_device->viewport();
    if (d3d_bridge_ok(result.status)) {
        wr32(v, result.viewport.x);
        wr32(v + 4, result.viewport.y);
        wr32(v + 8, result.viewport.width);
        wr32(v + 12, result.viewport.height);
        memcpy(gm_ptr(v + 16), &result.viewport.min_z, 4);
        memcpy(gm_ptr(v + 20), &result.viewport.max_z, 4);
    }
    com_ret(c, bridge_result(c, result.status));
}

// (this, State, pMatrix): world, view, projection and texture transforms,
// kept for the fixed-function path.
void Dev_SetTransform(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t state = arg(c, 1), m = arg(c, 2);
    if (!dev || !dev->d3d9_device || !m || !gm_valid(m, 64)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    float matrix[16];
    memcpy(matrix, gm_ptr(m), 64);
    com_ret(c, bridge_result(c, dev->d3d9_device->set_transform(
                                    state, rust::Slice<const float>(matrix, 16))));
}

// (this, State, pMatrix): multiply the current transform by the supplied
// matrix, current = current * supplied, in D3D9's row-major layout.
void Dev_MultiplyTransform(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t state = arg(c, 1), m = arg(c, 2);
    if (!dev || !dev->d3d9_device || !m || !gm_valid(m, 64)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    float matrix[16];
    memcpy(matrix, gm_ptr(m), 64);
    com_ret(c, bridge_result(c, dev->d3d9_device->multiply_transform(
                                    state, rust::Slice<const float>(matrix, 16))));
}

// (this, Stage, Type, Value)
void Dev_SetTextureStageState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t stage = arg(c, 1), type = arg(c, 2);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->set_texture_stage_state(stage, type, arg(c, 3))));
}

void Dev_SetRenderState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t state = arg(c, 1);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->set_render_state(state, arg(c, 2))));
}

// (this, Sampler, Type, Value)
void Dev_SetSamplerState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t sampler = arg(c, 1), type = arg(c, 2);
    if (!dev) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!dev->d3d9_device || sampler >= 16) {
        if (sampler >= D3DDMAPSAMPLER && sampler <= D3DVERTEXTEXTURESAMPLER3)
            imports_unsupported(c);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->set_sampler_state(sampler, type, arg(c, 3))));
}

// (this, StartRegister, pConstantData, Vector4fCount)
void Dev_SetVertexShaderConstantF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t start = arg(c, 1), data = arg(c, 2), count = arg(c, 3);
    if (!dev || !dev->d3d9_device || count > 256 || start > 256 - count) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!data || !gm_valid(data, count * 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    std::vector<float> values(count * 4);
    memcpy(values.data(), gm_ptr(data), count * 16);
    com_ret(c, bridge_result(
                   c, dev->d3d9_device->set_shader_constants_f(
                          false, start, rust::Slice<const float>(values.data(), values.size()))));
}
void Dev_SetPixelShaderConstantF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t start = arg(c, 1), data = arg(c, 2), count = arg(c, 3);
    bool range_valid = count <= D3D9_PIXEL_SHADER_FLOAT_CONSTANTS &&
                       start <= D3D9_PIXEL_SHADER_FLOAT_CONSTANTS - count;
    bool data_valid = !count || (count <= UINT32_MAX / 16 && data && gm_valid(data, count * 16));
    if (!dev || !dev->d3d9_device || !range_valid || !data_valid) {
        static uint32_t failures = 0;
        if (++failures == 1 || (failures & (failures - 1)) == 0)
            LOGW("d3d9: SetPixelShaderConstantF invalid upload: start=%u count=%u data=%08x "
                 "limit=%u range_valid=%d data_valid=%d failures=%u",
                 start, count, data, D3D9_PIXEL_SHADER_FLOAT_CONSTANTS, range_valid, data_valid,
                 failures);
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    std::vector<float> values(count * 4);
    memcpy(values.data(), gm_ptr(data), count * 16);
    com_ret(c, bridge_result(
                   c, dev->d3d9_device->set_shader_constants_f(
                          true, start, rust::Slice<const float>(values.data(), values.size()))));
}

// The DLL's one export. Not a COM method: the guest calls it directly.
void d3d9_Direct3DCreate9(X86 *c) {
    ComObj *o = com_new(K_D3D9);
    uint32_t view = o ? com_view(o, IF_D3D9) : 0;
    if (!view) {
        if (o)
            com_release(o);
        set_eax(c, 0);
        return;
    }
    LOGW("d3d9: Direct3DCreate9(SDK %u) -> %08x", arg(c, 0), view);
    set_eax(c, view);
}

static const ImportShim g_d3d9_exports[] = {
    {"d3d9.dll", "Direct3DCreate9", 1, d3d9_Direct3DCreate9},
};

void d3d9_reset() {
    g_scratch = 0;
    g_scratch_size = 0;
}

static bool d3d9_write_view(uint32_t out, uint32_t id, ComIface iface) {
    ComObj *o = com_get(id);
    if (!o)
        return false;
    com_addref(o);
    uint32_t view = com_view(o, iface);
    if (!view) {
        com_release(o);
        return false;
    }
    com_out_ptr(out, view);
    return true;
}

static void d3d9_stage_shader(uint32_t addr, std::vector<uint32_t> &out) {
    if (!addr)
        return;
    for (uint32_t i = 0; i < 4096; ++i) {
        if (addr > UINT32_MAX - i * 4u)
            break;
        uint32_t a = addr + i * 4u;
        if (!gm_valid(a, 4))
            break;
        out.push_back(rd32(a));
    }
}

static uint32_t d3d9_shader_id(ComObj *dev, uint32_t handle, bool pixel) {
    if (!dev || !dev->d3d9_device || !handle)
        return 0;
    auto &map = dev->d3d9_device->shader_views;
    auto it = map.find(handle);
    return it == map.end() ? 0 : it->second;
}

void Dev_GetRenderState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->render_state(arg(c, 1));
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetTextureStageState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 3);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->texture_stage_state(arg(c, 1), arg(c, 2));
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetSamplerState(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 3);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->sampler_state(arg(c, 1), arg(c, 2));
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetTransform(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || (!out || !gm_valid(out, 64))) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::FloatValuesResult result = dev->d3d9_device->transform(arg(c, 1));
    if (d3d_bridge_ok(result.status) && result.values.size() == 16)
        for (size_t i = 0; i < 16; ++i)
            memcpy(gm_ptr(out + (uint32_t)i * 4), &result.values[i], 4);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetScissorRect(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9ScissorResult result = dev->d3d9_device->scissor();
    if (d3d_bridge_ok(result.status)) {
        wr32(out, (uint32_t)result.left);
        wr32(out + 4, (uint32_t)result.top);
        wr32(out + 8, (uint32_t)result.right);
        wr32(out + 12, (uint32_t)result.bottom);
    }
    com_ret(c, bridge_result(c, result.status));
}
void Dev_SetMaterial(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 1);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 68)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    float values[17];
    memcpy(values, gm_ptr(p), 68);
    com_ret(c,
            bridge_result(c, dev->d3d9_device->set_material(rust::Slice<const float>(values, 17))));
}
void Dev_GetMaterial(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 1);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 68)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::FloatValuesResult result = dev->d3d9_device->material();
    if (d3d_bridge_ok(result.status) && result.values.size() == 17)
        memcpy(gm_ptr(p), result.values.data(), 68);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_SetLight(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 2);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 104)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t type = rd32(p);
    float values[25];
    memcpy(values, gm_ptr(p + 4), 100);
    com_ret(c, bridge_result(c, dev->d3d9_device->set_light(arg(c, 1), type,
                                                            rust::Slice<const float>(values, 25))));
}
void Dev_GetLight(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 2);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 104)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9LightResult result = dev->d3d9_device->light(arg(c, 1));
    if (d3d_bridge_ok(result.status) && result.values.size() == 25) {
        wr32(p, result.light_type);
        memcpy(gm_ptr(p + 4), result.values.data(), 100);
    }
    com_ret(c, bridge_result(c, result.status));
}
void Dev_LightEnable(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->set_light_enable(arg(c, 1), arg(c, 2) != 0)));
}
void Dev_GetLightEnable(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->light_enable(arg(c, 1));
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_SetClipPlane(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 2);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    float values[4];
    memcpy(values, gm_ptr(p), 16);
    com_ret(c, bridge_result(c, dev->d3d9_device->set_clip_plane(
                                    arg(c, 1), rust::Slice<const float>(values, 4))));
}
void Dev_GetClipPlane(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t p = arg(c, 2);
    if (!dev || !dev->d3d9_device || !p || !gm_valid(p, 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::FloatValuesResult result = dev->d3d9_device->clip_plane(arg(c, 1));
    if (d3d_bridge_ok(result.status) && result.values.size() == 4)
        memcpy(gm_ptr(p), result.values.data(), 16);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetTexture(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->texture(arg(c, 1));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!result.value || !d3d9_write_view(out, result.value, IF_D3DBASETEXTURE9))
        wr32(out, 0);
    com_ret(c, D3D_OK9);
}
void Dev_GetStreamSource(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2), offset = arg(c, 3), stride = arg(c, 4);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4) || !offset || !gm_valid(offset, 4) ||
        !stride || !gm_valid(stride, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9StreamResult result = dev->d3d9_device->stream_source(arg(c, 1));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!result.buffer || !d3d9_write_view(out, result.buffer, IF_D3DVERTEXBUFFER9))
        wr32(out, 0);
    wr32(offset, result.offset);
    wr32(stride, result.stride);
    com_ret(c, D3D_OK9);
}
void Dev_SetStreamSourceFreq(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    com_ret(c, bridge_result(c, dev->d3d9_device->set_stream_freq(arg(c, 1), arg(c, 2))));
}
void Dev_GetStreamSourceFreq(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->stream_freq(arg(c, 1));
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetIndices(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->indices();
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!result.value || !d3d9_write_view(out, result.value, IF_D3DINDEXBUFFER9))
        wr32(out, 0);
    com_ret(c, D3D_OK9);
}
void Dev_GetFVF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->fvf();
    if (d3d_bridge_ok(result.status))
        wr32(out, result.value);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_SetFVF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t fvf = arg(c, 1);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!fvf) {
        com_ret(c, D3D_OK9);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->create_fvf_declaration(fvf);
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    if (!d3d9_fvf_decl_view(dev, result.value)) {
        dev->d3d9_device->destroy_declaration(result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->set_fvf_binding(result.value, fvf);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}
void Dev_GetVertexDeclaration(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->vertex_declaration();
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t id = 0;
    if (result.value) {
        auto it = dev->d3d9_device->decl_views.find(result.value);
        if (it != dev->d3d9_device->decl_views.end())
            id = it->second;
    }
    if (!id || !d3d9_write_view(out, id, IF_D3DVERTEXDECL9))
        wr32(out, 0);
    com_ret(c, D3D_OK9);
}
void Dev_CreateVertexShader(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    std::vector<uint32_t> code;
    d3d9_stage_shader(arg(c, 1), code);
    recomp::d3d::ValueResult result = dev->d3d9_device->create_shader(
        false, rust::Slice<const uint32_t>(code.data(), code.size()));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *o = d3d9_make_object(dev, K_D3D9VS, IF_D3DVERTEXSHADER9, result.value, out);
    if (!o) {
        dev->d3d9_device->destroy_shader(false, result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->d3d9_device->shader_views[result.value] = o->id;
    com_ret(c, D3D_OK9);
}
void Dev_SetVertexShader(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *s = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || (arg(c, 1) && (!s || s->kind != K_D3D9VS)) ||
        (s && s->dev_d3d != dev->id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->set_shader(false, s ? s->d3d9_state_handle : 0);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}
void Dev_GetVertexShader(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->shader(false);
    uint32_t id = d3d9_shader_id(dev, d3d_bridge_ok(result.status) ? result.value : 0, false);
    if (!id || !d3d9_write_view(out, id, IF_D3DVERTEXSHADER9))
        wr32(out, 0);
    com_ret(c, D3D_OK9);
}
void Dev_CreatePixelShader(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    std::vector<uint32_t> code;
    d3d9_stage_shader(arg(c, 1), code);
    recomp::d3d::ValueResult result = dev->d3d9_device->create_shader(
        true, rust::Slice<const uint32_t>(code.data(), code.size()));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *o = d3d9_make_object(dev, K_D3D9PS, IF_D3DPIXELSHADER9, result.value, out);
    if (!o) {
        dev->d3d9_device->destroy_shader(true, result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->d3d9_device->shader_views[result.value] = o->id;
    com_ret(c, D3D_OK9);
}
void Dev_SetPixelShader(X86 *c) {
    ComObj *dev = this_device9(c);
    ComObj *s = com_this(arg(c, 1));
    if (!dev || !dev->d3d9_device || (arg(c, 1) && (!s || s->kind != K_D3D9PS)) ||
        (s && s->dev_d3d != dev->id)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->set_shader(true, s ? s->d3d9_state_handle : 0);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}
void Dev_GetPixelShader(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValueResult result = dev->d3d9_device->shader(true);
    uint32_t id = d3d9_shader_id(dev, d3d_bridge_ok(result.status) ? result.value : 0, true);
    if (!id || !d3d9_write_view(out, id, IF_D3DPIXELSHADER9))
        wr32(out, 0);
    com_ret(c, D3D_OK9);
}
static void d3d9_get_function(X86 *c, bool pixel) {
    ComObj *o = com_this_arg(c);
    uint32_t data = arg(c, 1), size = arg(c, 2);
    if (!o || o->kind != (pixel ? K_D3D9PS : K_D3D9VS) || !size || !gm_valid(size, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(o->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::ValuesResult result =
        dev->d3d9_device->shader_function(pixel, o->d3d9_state_handle);
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t required = (uint32_t)result.values.size() * 4;
    uint32_t available = rd32(size);
    wr32(size, required);
    if (!data) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (available < required || !gm_valid(data, required)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    for (size_t i = 0; i < result.values.size(); ++i)
        wr32(data + (uint32_t)i * 4, result.values[i]);
    com_ret(c, D3D_OK9);
}
void VS_GetFunction(X86 *c) {
    d3d9_get_function(c, false);
}
void PS_GetFunction(X86 *c) {
    d3d9_get_function(c, true);
}
void Decl_GetDeclaration(X86 *c) {
    ComObj *o = com_this_arg(c);
    uint32_t data = arg(c, 1), count = arg(c, 2);
    if (!o || o->kind != K_D3D9DECL || !count || !gm_valid(count, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(o->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::D3d9DeclarationResult result =
        dev->d3d9_device->declaration_elements(o->d3d9_state_handle);
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    uint32_t required = (uint32_t)result.elements.size();
    uint32_t available = rd32(count);
    wr32(count, required);
    if (!data) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (available < required || !gm_valid(data, required * 8)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    for (uint32_t i = 0; i < required; ++i) {
        const recomp::d3d::D3d9VertexElement &e = result.elements[i];
        uint32_t at = data + i * 8;
        wr16(at, e.stream);
        wr16(at + 2, e.offset);
        gm_ptr(at)[4] = e.element_type;
        gm_ptr(at)[5] = e.method;
        gm_ptr(at)[6] = e.usage;
        gm_ptr(at)[7] = e.usage_index;
    }
    com_ret(c, D3D_OK9);
}
static void d3d9_constant_i(X86 *c, bool pixel, bool set) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t start = arg(c, 1), data = arg(c, 2), count = arg(c, 3);
    if (count > 16 || start > 16 - count) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!data || !gm_valid(data, count * 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (set) {
        std::vector<int32_t> values(count * 4);
        memcpy(values.data(), gm_ptr(data), count * 16);
        com_ret(c, bridge_result(c, dev->d3d9_device->set_shader_constants_i(
                                        pixel, start,
                                        rust::Slice<const int32_t>(values.data(), values.size()))));
        return;
    }
    recomp::d3d::ValuesResult result = dev->d3d9_device->shader_constants_i(pixel, start, count);
    if (d3d_bridge_ok(result.status) && result.values.size() == (size_t)count * 4)
        for (uint32_t i = 0; i < count * 4; ++i)
            wr32(data + i * 4, result.values[i]);
    com_ret(c, bridge_result(c, result.status));
}
static void d3d9_constant_b(X86 *c, bool pixel, bool set) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    uint32_t start = arg(c, 1), data = arg(c, 2), count = arg(c, 3);
    if (count > 16 || start > 16 - count) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!data || !gm_valid(data, count * 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (set) {
        std::vector<uint32_t> values(count);
        for (uint32_t i = 0; i < count; ++i)
            values[i] = rd32(data + i * 4);
        com_ret(c,
                bridge_result(c, dev->d3d9_device->set_shader_constants_b(
                                     pixel, start,
                                     rust::Slice<const uint32_t>(values.data(), values.size()))));
        return;
    }
    recomp::d3d::ValuesResult result = dev->d3d9_device->shader_constants_b(pixel, start, count);
    if (d3d_bridge_ok(result.status) && result.values.size() == count)
        for (uint32_t i = 0; i < count; ++i)
            wr32(data + i * 4, result.values[i]);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetVertexShaderConstantF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t data = arg(c, 2), count = arg(c, 3);
    if (!dev || !dev->d3d9_device || count > 256 || arg(c, 1) > 256 - count) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!data || !gm_valid(data, count * 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::FloatValuesResult result =
        dev->d3d9_device->shader_constants_f(false, arg(c, 1), count);
    if (d3d_bridge_ok(result.status) && result.values.size() == (size_t)count * 4)
        memcpy(gm_ptr(data), result.values.data(), count * 16);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_GetPixelShaderConstantF(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t data = arg(c, 2), count = arg(c, 3);
    if (!dev || !dev->d3d9_device || count > D3D9_PIXEL_SHADER_FLOAT_CONSTANTS ||
        arg(c, 1) > D3D9_PIXEL_SHADER_FLOAT_CONSTANTS - count) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    if (!count) {
        com_ret(c, D3D_OK9);
        return;
    }
    if (!data || !gm_valid(data, count * 16)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::FloatValuesResult result =
        dev->d3d9_device->shader_constants_f(true, arg(c, 1), count);
    if (d3d_bridge_ok(result.status) && result.values.size() == (size_t)count * 4)
        memcpy(gm_ptr(data), result.values.data(), count * 16);
    com_ret(c, bridge_result(c, result.status));
}
void Dev_SetVertexShaderConstantI(X86 *c) {
    d3d9_constant_i(c, false, true);
}
void Dev_GetVertexShaderConstantI(X86 *c) {
    d3d9_constant_i(c, false, false);
}
void Dev_SetPixelShaderConstantI(X86 *c) {
    d3d9_constant_i(c, true, true);
}
void Dev_GetPixelShaderConstantI(X86 *c) {
    d3d9_constant_i(c, true, false);
}
void Dev_SetVertexShaderConstantB(X86 *c) {
    d3d9_constant_b(c, false, true);
}
void Dev_GetVertexShaderConstantB(X86 *c) {
    d3d9_constant_b(c, false, false);
}
void Dev_SetPixelShaderConstantB(X86 *c) {
    d3d9_constant_b(c, true, true);
}
void Dev_GetPixelShaderConstantB(X86 *c) {
    d3d9_constant_b(c, true, false);
}
void Dev_CreateStateBlock(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 2);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    recomp::d3d::ValueResult result = dev->d3d9_device->create_state_block(arg(c, 1));
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *b = d3d9_make_object(dev, K_D3D9STATEBLOCK, IF_D3DSTATEBLOCK9, result.value, out);
    if (!b) {
        dev->d3d9_device->destroy_state_block(result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->d3d9_device->block_views[result.value] = b->id;
    d3d9_refresh_block_bindings(dev, result.value);
    com_ret(c, D3D_OK9);
}
void Dev_BeginStateBlock(X86 *c) {
    ComObj *dev = this_device9(c);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->begin_state_block();
    if (d3d_bridge_ok(status))
        dev->d3d9_device->recording_block = true;
    com_ret(c, bridge_result(c, status));
}
void Dev_EndStateBlock(X86 *c) {
    ComObj *dev = this_device9(c);
    uint32_t out = arg(c, 1);
    if (!dev || !dev->d3d9_device || !out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    wr32(out, 0);
    recomp::d3d::ValueResult result = dev->d3d9_device->end_state_block();
    if (d3d_bridge_ok(result.status))
        dev->d3d9_device->recording_block = false;
    if (!d3d_bridge_ok(result.status)) {
        com_ret(c, bridge_result(c, result.status));
        return;
    }
    ComObj *b = d3d9_make_object(dev, K_D3D9STATEBLOCK, IF_D3DSTATEBLOCK9, result.value, out);
    if (!b) {
        d3d9_refresh_bindings(dev);
        dev->d3d9_device->destroy_state_block(result.value);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    dev->d3d9_device->block_views[result.value] = b->id;
    d3d9_refresh_block_bindings(dev, result.value);
    d3d9_refresh_bindings(dev);
    com_ret(c, D3D_OK9);
}
void SB_Capture(X86 *c) {
    ComObj *b = com_this_arg(c);
    if (!b || b->kind != K_D3D9STATEBLOCK) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(b->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->capture_state_block(b->d3d9_state_handle);
    if (d3d_bridge_ok(status))
        d3d9_refresh_block_bindings(dev, b->d3d9_state_handle);
    com_ret(c, bridge_result(c, status));
}
void SB_Apply(X86 *c) {
    ComObj *b = com_this_arg(c);
    if (!b || b->kind != K_D3D9STATEBLOCK) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    ComObj *dev = com_get(b->dev_d3d);
    if (!dev || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALL);
        return;
    }
    recomp::d3d::Status status = dev->d3d9_device->apply_state_block(b->d3d9_state_handle);
    if (d3d_bridge_ok(status))
        d3d9_refresh_bindings(dev);
    com_ret(c, bridge_result(c, status));
}

#include "d3d9_interfaces.inc"

static void d3d9_drop_fvf_cache(ComObj *dev) {
    if (!dev || !dev->d3d9_device)
        return;
    for (uint32_t id : dev->d3d9_device->fvf_decls) {
        if (ComObj *o = com_get(id))
            com_release_internal(o);
    }
    dev->d3d9_device->fvf_decls.clear();
}

static void d3d9_device_destroy(ComObj *o) {
    gdi_forget_surface(o->id);
    device_discard_implicit_surfaces(o);
    d3d9_clear_virtual_display(o);
    if (o->d3d9_device) {
        if (X86 *c = guest_current_context())
            win32_uncover_display(c, o->hwnd);
        d3d9_drop_all_refs(o);
        d3d9_drop_fvf_cache(o);
        o->d3d9_device.reset();
        if (ComObj *factory = com_get(o->dev_d3d))
            com_release(factory);
    }
    o->dev_d3d = 0;
    o->d3d9_focus_window = 0;
}

static void d3d9_child_refs_changed(ComObj *o) {
    if (!o || !o->dev_d3d)
        return;
    ComObj *dev = com_get(o->dev_d3d);
    if (!dev || !dev->d3d9_device)
        return;
    const bool external = o->refs > o->internal_refs;
    if (external == o->d3d9_owner_retained)
        return;
    o->d3d9_owner_retained = external;
    if (external)
        com_retain_internal(dev);
    else
        com_release_internal(dev);
}

static void d3d9_pipeline_object_destroy(ComObj *o) {
    if (!o)
        return;
    ComObj *dev = com_get(o->dev_d3d);
    if (dev && dev->d3d9_device) {
        D3d9HostDevice *host = dev->d3d9_device.get();
        switch (o->kind) {
        case K_D3D9VS:
            host->shader_views.erase(o->d3d9_state_handle);
            host->destroy_shader(false, o->d3d9_state_handle);
            break;
        case K_D3D9PS:
            host->shader_views.erase(o->d3d9_state_handle);
            host->destroy_shader(true, o->d3d9_state_handle);
            break;
        case K_D3D9DECL:
            host->decl_views.erase(o->d3d9_state_handle);
            host->fvf_decls.erase(o->id);
            host->destroy_declaration(o->d3d9_state_handle);
            break;
        case K_D3D9STATEBLOCK:
            d3d9_drop_block_refs(dev, o->d3d9_state_handle);
            host->block_views.erase(o->d3d9_state_handle);
            host->destroy_state_block(o->d3d9_state_handle);
            break;
        default:
            break;
        }
    }
    if (o->d3d9_owner_retained) {
        o->d3d9_owner_retained = false;
        if (dev)
            com_release_internal(dev);
    }
    o->dev_d3d = 0;
    o->d3d9_state_handle = 0;
}

void d3d9_register() {
    static bool done = false;
    if (done)
        return;
    done = true;

    com_define_checked<IF_D3D9, g_d3d9>("d3d9.dll", "IDirect3D9");
    com_define_checked<IF_D3DDEVICE9, g_device9>("d3d9.dll", "IDirect3DDevice9");
    com_define_checked<IF_D3DRESOURCE9, g_resource9>("d3d9.dll", "IDirect3DResource9");
    com_bind(IF_D3D9, K_D3D9);
    com_bind(IF_D3DDEVICE9, K_D3D9DEVICE);
    com_bind(IF_D3DRESOURCE9, K_D3D9VB);
    com_bind(IF_D3DRESOURCE9, K_D3D9IB);
    com_register_iid(IF_D3D9, IID_IDirect3D9_);
    com_register_iid(IF_D3DDEVICE9, IID_IDirect3DDevice9_);
    com_register_iid(IF_D3DRESOURCE9, IID_IDirect3DResource9_);
    com_define_checked<IF_D3DTEXTURE9, g_texture9>("d3d9.dll", "IDirect3DTexture9");
    com_define_checked<IF_D3DCUBETEXTURE9, g_cubetexture9>("d3d9.dll", "IDirect3DCubeTexture9");
    com_define_checked<IF_D3DSURFACE9, g_surface9>("d3d9.dll", "IDirect3DSurface9");
    com_define_checked<IF_D3DVERTEXBUFFER9, g_vb9>("d3d9.dll", "IDirect3DVertexBuffer9");
    com_define_checked<IF_D3DINDEXBUFFER9, g_ib9>("d3d9.dll", "IDirect3DIndexBuffer9");
    com_define_checked<IF_D3DVERTEXDECL9, g_decl9>("d3d9.dll", "IDirect3DVertexDeclaration9");
    com_define_checked<IF_D3DQUERY9, g_query9>("d3d9.dll", "IDirect3DQuery9");
    com_define_checked<IF_D3DBASETEXTURE9, g_basetexture9>("d3d9.dll", "IDirect3DBaseTexture9");
    com_define_checked<IF_D3DVOLUME9, g_volume9>("d3d9.dll", "IDirect3DVolume9");
    com_define_checked<IF_D3DVOLUMETEXTURE9, g_volumetexture9>("d3d9.dll",
                                                               "IDirect3DVolumeTexture9");
    com_define_checked<IF_D3DVERTEXSHADER9, g_vertexshader9>("d3d9.dll", "IDirect3DVertexShader9");
    com_define_checked<IF_D3DPIXELSHADER9, g_pixelshader9>("d3d9.dll", "IDirect3DPixelShader9");
    com_define_checked<IF_D3DSTATEBLOCK9, g_stateblock9>("d3d9.dll", "IDirect3DStateBlock9");
    com_register_iid(IF_D3DTEXTURE9, IID_IDirect3DTexture9_);
    com_register_iid(IF_D3DCUBETEXTURE9, IID_IDirect3DCubeTexture9_);
    com_register_iid(IF_D3DSURFACE9, IID_IDirect3DSurface9_);
    com_register_iid(IF_D3DVERTEXBUFFER9, IID_IDirect3DVertexBuffer9_);
    com_register_iid(IF_D3DINDEXBUFFER9, IID_IDirect3DIndexBuffer9_);
    com_register_iid(IF_D3DVERTEXDECL9, IID_IDirect3DVertexDeclaration9_);
    com_register_iid(IF_D3DQUERY9, IID_IDirect3DQuery9_);
    com_register_iid(IF_D3DBASETEXTURE9, IID_IDirect3DBaseTexture9_);
    com_register_iid(IF_D3DVOLUME9, IID_IDirect3DVolume9_);
    com_register_iid(IF_D3DVOLUMETEXTURE9, IID_IDirect3DVolumeTexture9_);
    com_register_iid(IF_D3DVERTEXSHADER9, IID_IDirect3DVertexShader9_);
    com_register_iid(IF_D3DPIXELSHADER9, IID_IDirect3DPixelShader9_);
    com_register_iid(IF_D3DSTATEBLOCK9, IID_IDirect3DStateBlock9_);
    com_bind(IF_D3DTEXTURE9, K_D3D9TEXTURE);
    com_bind(IF_D3DCUBETEXTURE9, K_D3D9CUBETEXTURE);
    com_bind(IF_D3DVOLUMETEXTURE9, K_D3D9VOLUMETEXTURE);
    com_bind(IF_D3DVOLUME9, K_D3D9VOLUME);
    com_bind(IF_D3DBASETEXTURE9, K_D3D9TEXTURE);
    com_bind(IF_D3DBASETEXTURE9, K_D3D9CUBETEXTURE);
    com_bind(IF_D3DBASETEXTURE9, K_D3D9VOLUMETEXTURE);
    com_bind(IF_D3DSURFACE9, K_D3D9SURFACE);
    com_bind(IF_D3DRESOURCE9, K_D3D9TEXTURE);
    com_bind(IF_D3DRESOURCE9, K_D3D9CUBETEXTURE);
    com_bind(IF_D3DRESOURCE9, K_D3D9VOLUMETEXTURE);
    com_bind(IF_D3DRESOURCE9, K_D3D9SURFACE);
    com_bind(IF_D3DVERTEXBUFFER9, K_D3D9VB);
    com_bind(IF_D3DINDEXBUFFER9, K_D3D9IB);
    com_bind(IF_D3DVERTEXDECL9, K_D3D9DECL);
    com_bind(IF_D3DQUERY9, K_D3D9QUERY);
    com_bind(IF_D3DVERTEXSHADER9, K_D3D9VS);
    com_bind(IF_D3DPIXELSHADER9, K_D3D9PS);
    com_bind(IF_D3DSTATEBLOCK9, K_D3D9STATEBLOCK);
    for (ComKind k : {K_D3D9TEXTURE, K_D3D9CUBETEXTURE, K_D3D9VOLUMETEXTURE, K_D3D9VOLUME,
                      K_D3D9SURFACE, K_D3D9VB, K_D3D9IB})
        com_set_destructor(k, d3d9_resource_destroy);
    for (ComKind k : {K_D3D9DECL, K_D3D9VS, K_D3D9PS, K_D3D9STATEBLOCK})
        com_set_destructor(k, d3d9_pipeline_object_destroy);
    for (ComKind k : {K_D3D9SURFACE, K_D3D9TEXTURE, K_D3D9CUBETEXTURE, K_D3D9VOLUMETEXTURE,
                      K_D3D9VB, K_D3D9IB, K_D3D9DECL, K_D3D9VS, K_D3D9PS, K_D3D9STATEBLOCK})
        com_set_ref_hook(k, d3d9_child_refs_changed);
    com_set_destructor(K_D3D9QUERY, query_destroy);
    com_set_destructor(K_D3D9DEVICE, d3d9_device_destroy);
    imports_register(g_d3d9_exports, std::size(g_d3d9_exports));
}
