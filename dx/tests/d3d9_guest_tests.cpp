#include "../dx.h"
#include "../d3d_bridge_helpers.h"
#include "../../runtime/memory.h"
#include "../../runtime/win32.h"
#include "guest_abi.h"

static void check(bool ok, const char *name) {
    ++g_checks;
    if (!ok) {
        ++g_failures;
        fprintf(stderr, "FAIL: %s\n", name);
    }
}

static uint32_t device_fixture(uint32_t factory) {
    uint32_t wc = sc(0x400), name = sc(0x480), present = sc(0x380);
    gm_zero(wc, 40);
    wr32(wc + 4, tramp("USER32.dll", "DefWindowProcA"));
    gm_put_str(name, "D3d9GuestFixture", 32);
    wr32(wc + 36, name);
    check(call_shim(tramp("USER32.dll", "RegisterClassA"), {wc}) != 0, "register window class");
    uint32_t window = call_shim(tramp("USER32.dll", "CreateWindowExA"),
                                {0, name, name, 0x80000000u, 0, 0, 16, 16, 0, 0, 0, 0});
    check(window != 0, "create guest window");
    gm_zero(present, 56);
    wr32(present, 16);
    wr32(present + 4, 16);
    wr32(present + 8, 21);
    wr32(present + 12, 1);
    wr32(present + 24, 1);
    wr32(present + 28, window);
    wr32(present + 32, 1);
    wr32(sc(0x3c0), 0);
    check(call_method(factory, 16, {0, 1, window, 0x40, present, sc(0x3c0)}) == 0,
          "guest CreateDevice");
    return rd32(sc(0x3c0));
}

int main() {
    mem_init();
    imports_init();
    win32_init(".");
    dx_register_shims();
    g_stack_top = STACK_TOP - 0x1000;
    g_scratch = heap_alloc(4096, true, 16);
    cpu_reset();
    uint32_t factory = call_shim(tramp("d3d9.dll", "Direct3DCreate9"), {32});
    check(factory != 0, "Direct3DCreate9 import");
    if (!factory)
        return 1;
    uint32_t device = device_fixture(factory);
    if (!device)
        return 1;
    constexpr uint32_t invalid = 0x8876086c;
    wr32(sc(0), 0xabcdef01);
    check(call_method(device, 26, {0, 0, 0x44, 1, sc(0), 0}) == invalid && rd32(sc(0)) == 0,
          "failed creation clears output");
    check(call_method(device, 26, {60, 0, 0x44, 1, sc(0), 0}) == 0, "create vertex buffer");
    uint32_t vb = rd32(sc(0));
    if (!vb)
        return 1;
    check(call_method(vb, 13, {sc(64)}) == 0 && rd32(sc(64)) == 100 && rd32(sc(80)) == 60 &&
              rd32(sc(84)) == 0x44,
          "vertex buffer descriptor");
    wr32(sc(4), 0xabcdef01);
    check(call_method(vb, 11, {61, 1, sc(4), 0}) == invalid && rd32(sc(4)) == 0xabcdef01,
          "out of range lock preserves output");
    check(call_method(vb, 11, {0, 0, GUEST_SIZE - 2, 0}) == invalid, "invalid lock output");
    check(call_method(vb, 11, {0, 0, sc(4), 0}) == 0, "lock vertex buffer");
    uint32_t vertices = rd32(sc(4));
    check(vertices && (vertices & 15) == 0, "aligned guest staging");
    check(call_method(vb, 11, {20, 20, sc(8), 0x10}) == 0 && rd32(sc(8)) == vertices + 20,
          "nested readonly lock shares staging");
    const float xy[3][2] = {{0, 0}, {0, 16}, {16, 0}};
    for (uint32_t i = 0; i < 3; ++i) {
        float v[4] = {xy[i][0], xy[i][1], 0.5f, 1.0f};
        memcpy(gm_ptr(vertices + i * 20), v, sizeof(v));
        wr32(vertices + i * 20 + 16, 0xffff0000);
    }
    check(call_method(vb, 12) == 0, "nested unlock");
    check(call_method(device, 100, {0, vb, 0, 20}) == 0, "bind locked buffer");
    check(call_method(device, 89, {0x44}) == 0, "set FVF");
    check(call_method(device, 57, {22, 1}) == 0, "disable culling");
    check(call_method(device, 43, {0, 0, 1, 0xff000000, 0x3f800000, 0}) == 0, "clear");
    check(call_method(device, 41) == 0, "begin scene");
    check(call_method(device, 81, {4, 0, 1}) == 0, "draw from locked guest staging");
    check(call_method(device, 42) == 0, "end scene");
    auto *object = com_this(device, IF_D3DDEVICE9);
    auto image = object->d3d9_device->present({}, {});
    check(image.status.code == recomp::d3d::StatusCode::Ok && image.bytes.size() == 16 * 16 * 4,
          "read rendered image");
    if (image.bytes.size() == 16 * 16 * 4)
        check(image.bytes[(6 * 16 + 6) * 4] == 255 && image.bytes[(6 * 16 + 6) * 4 + 1] == 0,
              "draw synchronized guest buffer writes");
    check(call_method(vb, 12) == 0, "final unlock");
    wr32(vertices + 16, 0xff00ff00);
    check(call_method(vb, 11, {20, 20, sc(4), 0}) == 0, "partial lock after retained write");
    check(call_method(vb, 12) == 0, "unlock retained write");
    check(call_method(vb, 11, {0, 0, sc(4), 0x10}) == 0 && rd32(sc(4)) == vertices,
          "retained staging reused");
    check(rd32(vertices + 16) == 0xff00ff00, "write outside lock range preserved");
    check(call_method(vb, 12) == 0, "readonly unlock");
    if (RECOMP_STORE_HOOKS) {
        g_dirty[0] = {sc(301), 24, UINT32_MAX, 0};
        g_dirty_count = 1;
        recomp_store_hook_update();
        check(call_method(vb, 13, {sc(301)}) == 0 && g_dirty[0].lo == sc(301) &&
                  g_dirty[0].hi == sc(325),
              "descriptor guest stores notify dirty tracking");
        g_dirty_count = 0;
        recomp_store_hook_update();
    }
    check(call_method(device, 27, {12, 8, 101, 1, sc(12), 0}) == 0,
          "create writeonly index buffer");
    uint32_t ib = rd32(sc(12));
    check(call_method(ib, 13, {sc(64)}) == 0 && rd32(sc(64)) == 101 && rd32(sc(80)) == 12,
          "index buffer descriptor");
    check(call_method(ib, 11, {0, 0, sc(4), 0x10}) == invalid, "writeonly rejects readonly lock");
    check(call_method(ib, 11, {0, 0, sc(4), 0}) == 0, "writeonly index lock");
    check(call_method(ib, 12) == 0, "index unlock");
    check(call_method(device, 104, {ib}) == 0, "bind index buffer");
    uint32_t ib_id = com_this(ib)->id;
    call_method(ib, 2);
    check(com_get(ib_id) != nullptr, "bound index retains COM identity");
    check(call_method(device, 104, {0}) == 0, "unbind index buffer");
    check(com_get(ib_id) == nullptr, "unbound index releases COM identity");
    check(call_method(device, 26, {128, 0, 0x44, 1, sc(32), 0}) == 0, "create OOM buffer");
    uint32_t oom_vb = rd32(sc(32));
    std::vector<uint32_t> exhausted;
    while (heap_stats().largest_free > 64) {
        uint32_t address = heap_alloc((uint32_t)heap_stats().largest_free - 64, false, 16);
        if (!address)
            break;
        exhausted.push_back(address);
    }
    wr32(sc(36), 0xabcdef01);
    check(call_method(oom_vb, 11, {0, 0, sc(36), 0}) == 0x8007000e && rd32(sc(36)) == 0xabcdef01,
          "guest staging OOM preserves output");
    for (uint32_t address : exhausted)
        heap_free(address);
    check(call_method(oom_vb, 11, {0, 0, sc(36), 0}) == 0, "lock succeeds after OOM");
    check(call_method(oom_vb, 12) == 0, "unlock after OOM");
    call_method(oom_vb, 2);
    wr32(sc(24), 0xabcdef01);
    check(call_method(device, 23, {0, 4, 2, 0, 21, 1, sc(24), 0}) == invalid && rd32(sc(24)) == 0,
          "invalid texture creation clears output");
    check(call_method(device, 23, {4, 4, 2, 0, 21, 1, sc(24), 0}) == 0, "create managed texture");
    uint32_t texture = rd32(sc(24));
    check(texture != 0, "texture view");
    if (!texture)
        return 1;
    check(call_method(texture, 17, {1, sc(101)}) == 0 && rd32(sc(105)) == 3 && rd32(sc(125)) == 2 &&
              rd32(sc(129)) == 2,
          "unaligned mip descriptor and texture type");
    check(call_method(texture, 18, {0, sc(28)}) == 0, "get level surface");
    uint32_t surface = rd32(sc(28));
    check(surface != 0, "surface view");
    if (!surface)
        return 1;
    wr32(sc(160), 1);
    wr32(sc(164), 1);
    wr32(sc(168), 3);
    wr32(sc(172), 3);
    check(call_method(texture, 19, {0, sc(180), sc(160), 0}) == 0 && rd32(sc(180)) == 16,
          "partial texture lock pitch");
    uint32_t texel = rd32(sc(184));
    wr32(texel, 0xff123456);
    check(call_method(surface, 13, {sc(188), 0, 0}) == invalid,
          "surface alias rejects duplicate lock");
    check(call_method(surface, 14) == 0, "surface alias unlocks texture lock");
    check(call_method(surface, 13, {sc(188), 0, 0x10}) == 0, "surface readonly lock");
    check(rd32(rd32(sc(192)) + 20) == 0xff123456, "texture update survives alias unlock");
    check(call_method(texture, 20, {0}) == 0, "texture unlocks surface lock");
    check(call_method(texture, 19, {0, sc(180), 0, 0}) == 0, "lock texture for GPU upload");
    uint32_t pixels = rd32(sc(184));
    for (uint32_t i = 0; i < 16; ++i)
        wr32(pixels + i * 4, 0xff2040e0);
    check(call_method(texture, 20, {0}) == 0, "upload texture");
    check(call_method(texture, 19, {0, sc(180), 0, 0}) == 0,
          "lock managed texture across rejected reset");
    pixels = rd32(sc(184));
    check(call_method(device, 26, {60, 0, 0x44, 0, sc(44), 0}) == 0,
          "create default reset blocker");
    uint32_t blocker = rd32(sc(44));
    check(call_method(device, 16, {sc(0x380)}) == invalid,
          "guest reset rejects live default buffer");
    check(rd32(pixels) == 0xff2040e0 && call_method(texture, 17, {1, sc(101)}) == 0 &&
              rd32(sc(125)) == 2 && rd32(sc(129)) == 2,
          "rejected reset preserves locked texture bytes and descriptor");
    call_method(blocker, 2);
    check(call_method(device, 16, {sc(0x380)}) == 0,
          "guest reset succeeds with managed texture locked");
    check(rd32(pixels) == 0xff2040e0 && call_method(texture, 19, {0, sc(180), 0, 0}) == invalid,
          "successful reset preserves managed staging and active lock");
    for (uint32_t i = 0; i < 16; ++i)
        wr32(pixels + i * 4, 0xffa0b0c0);
    check(call_method(texture, 20, {0}) == 0, "unlock managed texture after successful reset");
    check(call_method(device, 100, {0, vb, 0, 20}) == 0 && call_method(device, 89, {0x44}) == 0 &&
              call_method(device, 57, {22, 1}) == 0,
          "restore draw state after reset");
    check(call_method(device, 65, {0, texture}) == 0, "bind uploaded texture");
    check(call_method(device, 67, {0, 1, 2}) == 0 && call_method(device, 67, {0, 2, 2}) == 0 &&
              call_method(device, 67, {0, 4, 2}) == 0 && call_method(device, 67, {0, 5, 2}) == 0,
          "select texture color and alpha");
    check(call_method(device, 43, {0, 0, 1, 0xff000000, 0x3f800000, 0}) == 0,
          "clear textured draw");
    check(call_method(device, 41) == 0 && call_method(device, 81, {4, 0, 1}) == 0 &&
              call_method(device, 42) == 0,
          "draw guest-uploaded texture");
    image = object->d3d9_device->present({}, {});
    check(image.status.code == recomp::d3d::StatusCode::Ok && image.bytes.size() == 16 * 16 * 4 &&
              image.bytes[(6 * 16 + 6) * 4] == 0xa0 && image.bytes[(6 * 16 + 6) * 4 + 1] == 0xb0 &&
              image.bytes[(6 * 16 + 6) * 4 + 2] == 0xc0,
          "sample guest-uploaded texture");
    check(call_method(device, 65, {0, 0}) == 0, "unbind texture");
    call_method(texture, 2);
    check(call_method(surface, 12, {sc(201)}) == 0 && rd32(sc(205)) == 3,
          "surface retains texture and descriptor type");
    call_method(surface, 2);
    check(call_method(device, 36, {4, 4, 21, 2, sc(40), 0}) == 0, "create offscreen surface");
    surface = rd32(sc(40));
    check(surface && call_method(surface, 12, {sc(201)}) == 0 && rd32(sc(205)) == 1,
          "offscreen surface descriptor type");
    check(call_method(surface, 13, {sc(188), 0, 0}) == 0, "lock offscreen surface");
    wr32(rd32(sc(192)), 0xff102030);
    check(call_method(surface, 14) == 0, "unlock offscreen surface");
    call_method(surface, 2);
    check(call_method(device, 100, {0, 0, 0, 0}) == 0, "unbind vertex buffer");
    call_method(vb, 2);
    call_method(factory, 2);
    check(call_method(device, 26, {60, 0, 0x44, 1, sc(0), 0}) == 0,
          "create buffer before heap reset");
    vb = rd32(sc(0));
    check(vb && call_method(vb, 11, {0, 0, sc(4), 0}) == 0, "lock across heap reset");
    check(call_method(device, 23, {4, 4, 2, 0, 21, 1, sc(24), 0}) == 0,
          "create texture before heap reset");
    texture = rd32(sc(24));
    check(texture && call_method(texture, 19, {0, sc(180), 0, 0}) == 0,
          "lock texture across heap reset");
    mem_init();
    dx_reset();
    g_scratch = heap_alloc(4096, true, 16);
    cpu_reset();
    factory = call_shim(tramp("d3d9.dll", "Direct3DCreate9"), {32});
    device = device_fixture(factory);
    check(device != 0, "device after guest heap reset");
    if (device) {
        check(call_method(device, 26, {60, 0, 0x44, 1, sc(0), 0}) == 0,
              "buffer after guest heap reset");
        vb = rd32(sc(0));
        check(vb && call_method(vb, 11, {0, 0, sc(4), 0}) == 0, "lock after heap reset");
        if (vb) {
            call_method(vb, 12);
            call_method(vb, 2);
        }
        call_method(device, 2);
        call_method(factory, 2);
    }
    fprintf(stderr, "d3d9 guest ABI: %d checks, %d failures\n", g_checks, g_failures);
    return g_failures ? 1 : 0;
}
