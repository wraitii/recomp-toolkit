// d3dx9.cpp - the D3DX 9 maths and the effect loader. The game uses the maths for its
// own culling and placement, so every function computes what the SDK documents. Effects
// are read from the executable's resources; the effect and pool objects are the Rust-backed
// ones in d3d9.cpp.
#include "com.h"
#include "dx.h"
#include "d3dx9tex.h"
#include "d3dx9shader_shim.h"
#include "d3dx9effect_shim.h"
#include "../runtime/guest.h"
#include "../runtime/memory.h"

#include <string.h>
#include <math.h>
#include <cctype>
#include <iterator>
#include <algorithm>
#include <string>

void X_D3DXDebugMute(X86 *c) {
    static uint32_t mute = 0;
    uint32_t previous = mute;
    mute = arg(c, 0);
    set_eax(c, previous);
}

// ---------------------------------------------------------------------------
// Guest floats
// ---------------------------------------------------------------------------
static float rf(uint32_t a) {
    uint32_t bits = rd32(a);
    float f;
    memcpy(&f, &bits, 4);
    return f;
}
static void wf(uint32_t a, float v) {
    uint32_t bits;
    memcpy(&bits, &v, 4);
    wr32(a, bits);
}
static void read_matrix(uint32_t p, float m[16]) {
    for (int i = 0; i < 16; ++i)
        m[i] = rf(p + 4u * (uint32_t)i);
}
static void write_matrix(uint32_t p, const float m[16]) {
    for (int i = 0; i < 16; ++i)
        wf(p + 4u * (uint32_t)i, m[i]);
}

// D3DX matrices are row-major and the SDK's functions return their output
// pointer, which callers chain on.
void X_D3DXMatrixMultiply(X86 *c) {
    uint32_t out = arg(c, 0), a = arg(c, 1), b = arg(c, 2);
    float A[16], B[16], R[16];
    read_matrix(a, A);
    read_matrix(b, B);
    for (int i = 0; i < 4; ++i)
        for (int j = 0; j < 4; ++j) {
            float s = 0.0f;
            for (int k = 0; k < 4; ++k)
                s += A[i * 4 + k] * B[k * 4 + j];
            R[i * 4 + j] = s;
        }
    write_matrix(out, R);
    set_eax(c, out);
}

void X_D3DXMatrixMultiplyTranspose(X86 *c) {
    uint32_t out = arg(c, 0), a = arg(c, 1), b = arg(c, 2);
    float A[16], B[16], R[16];
    read_matrix(a, A);
    read_matrix(b, B);
    for (int i = 0; i < 4; ++i)
        for (int j = 0; j < 4; ++j) {
            float s = 0.0f;
            for (int k = 0; k < 4; ++k)
                s += A[i * 4 + k] * B[k * 4 + j];
            R[j * 4 + i] = s;
        }
    write_matrix(out, R);
    set_eax(c, out);
}

void X_D3DXCheckVersion(X86 *c) {
    uint32_t d3d = arg(c, 0), d3dx = arg(c, 1);
    if (d3d != 32 || d3dx < 26)
        LOGW("d3dx9: D3DXCheckVersion(D3D %u, D3DX %u): unsupported SDK version", d3d, d3dx);
    set_eax(c, d3d == 32 && d3dx >= 26 ? 1 : 0);
}

void X_D3DXMatrixTranspose(X86 *c) {
    uint32_t out = arg(c, 0), in = arg(c, 1);
    float M[16], R[16];
    read_matrix(in, M);
    for (int i = 0; i < 4; ++i)
        for (int j = 0; j < 4; ++j)
            R[i * 4 + j] = M[j * 4 + i];
    write_matrix(out, R);
    set_eax(c, out);
}

// The cofactor expansion, with the determinant written out when asked for.
void X_D3DXMatrixInverse(X86 *c) {
    uint32_t out = arg(c, 0), pdet = arg(c, 1), in = arg(c, 2);
    float m[16], inv[16];
    read_matrix(in, m);
    inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15] +
             m[9] * m[7] * m[14] + m[13] * m[6] * m[11] - m[13] * m[7] * m[10];
    inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15] -
             m[8] * m[7] * m[14] - m[12] * m[6] * m[11] + m[12] * m[7] * m[10];
    inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15] +
             m[8] * m[7] * m[13] + m[12] * m[5] * m[11] - m[12] * m[7] * m[9];
    inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14] -
              m[8] * m[6] * m[13] - m[12] * m[5] * m[10] + m[12] * m[6] * m[9];
    inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15] -
             m[9] * m[3] * m[14] - m[13] * m[2] * m[11] + m[13] * m[3] * m[10];
    inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15] +
             m[8] * m[3] * m[14] + m[12] * m[2] * m[11] - m[12] * m[3] * m[10];
    inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15] -
             m[8] * m[3] * m[13] - m[12] * m[1] * m[11] + m[12] * m[3] * m[9];
    inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14] +
              m[8] * m[2] * m[13] + m[12] * m[1] * m[10] - m[12] * m[2] * m[9];
    inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15] + m[5] * m[3] * m[14] +
             m[13] * m[2] * m[7] - m[13] * m[3] * m[6];
    inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15] -
             m[4] * m[3] * m[14] - m[12] * m[2] * m[7] + m[12] * m[3] * m[6];
    inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15] +
              m[4] * m[3] * m[13] + m[12] * m[1] * m[7] - m[12] * m[3] * m[5];
    inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14] -
              m[4] * m[2] * m[13] - m[12] * m[1] * m[6] + m[12] * m[2] * m[5];
    inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11] -
             m[5] * m[3] * m[10] - m[9] * m[2] * m[7] + m[9] * m[3] * m[6];
    inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11] + m[4] * m[3] * m[10] +
             m[8] * m[2] * m[7] - m[8] * m[3] * m[6];
    inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11] - m[4] * m[3] * m[9] -
              m[8] * m[1] * m[7] + m[8] * m[3] * m[5];
    inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10] + m[4] * m[2] * m[9] +
              m[8] * m[1] * m[6] - m[8] * m[2] * m[5];
    float det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
    if (pdet)
        wf(pdet, det);
    if (det == 0.0f) {
        set_eax(c, 0); // the SDK returns NULL for a singular matrix
        return;
    }
    float r = 1.0f / det;
    for (int i = 0; i < 16; ++i)
        inv[i] *= r;
    write_matrix(out, inv);
    set_eax(c, out);
}

void X_D3DXMatrixTranslation(X86 *c) {
    uint32_t out = arg(c, 0);
    float m[16] = {1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1};
    uint32_t xb = arg(c, 1), yb = arg(c, 2), zb = arg(c, 3);
    memcpy(&m[12], &xb, 4);
    memcpy(&m[13], &yb, 4);
    memcpy(&m[14], &zb, 4);
    write_matrix(out, m);
    set_eax(c, out);
}

// Left-handed projections, exactly as the SDK builds them. The float
// arguments arrive as dwords on the stack.
static float argf(X86 *c, int n) {
    uint32_t bits = arg(c, (uint32_t)n);
    float f;
    memcpy(&f, &bits, 4);
    return f;
}

void X_D3DXMatrixOrthoLH(X86 *c) {
    uint32_t out = arg(c, 0);
    float w = argf(c, 1), h = argf(c, 2), zn = argf(c, 3), zf = argf(c, 4);
    float m[16] = {0};
    m[0] = 2.0f / w;
    m[5] = 2.0f / h;
    m[10] = 1.0f / (zf - zn);
    m[14] = zn / (zn - zf);
    m[15] = 1.0f;
    write_matrix(out, m);
    set_eax(c, out);
}

void X_D3DXMatrixPerspectiveLH(X86 *c) {
    uint32_t out = arg(c, 0);
    float w = argf(c, 1), h = argf(c, 2), zn = argf(c, 3), zf = argf(c, 4);
    float m[16] = {0};
    m[0] = 2.0f * zn / w;
    m[5] = 2.0f * zn / h;
    m[10] = zf / (zf - zn);
    m[11] = 1.0f;
    m[14] = -zn * zf / (zf - zn);
    write_matrix(out, m);
    set_eax(c, out);
}

// Vectors. Transform takes a 3-vector to a 4-vector; TransformCoord divides
// through by w; TransformNormal ignores translation.
void X_D3DXVec4Transform(X86 *c) {
    uint32_t out = arg(c, 0), v = arg(c, 1), pm = arg(c, 2);
    float m[16], in[4], r[4];
    read_matrix(pm, m);
    for (int i = 0; i < 4; ++i)
        in[i] = rf(v + 4u * (uint32_t)i);
    for (int j = 0; j < 4; ++j)
        r[j] = in[0] * m[j] + in[1] * m[4 + j] + in[2] * m[8 + j] + in[3] * m[12 + j];
    for (int i = 0; i < 4; ++i)
        wf(out + 4u * (uint32_t)i, r[i]);
    set_eax(c, out);
}

void X_D3DXVec3Transform(X86 *c) {
    uint32_t out = arg(c, 0), v = arg(c, 1), pm = arg(c, 2);
    float m[16], x = rf(v), y = rf(v + 4), z = rf(v + 8);
    read_matrix(pm, m);
    for (int j = 0; j < 4; ++j)
        wf(out + 4u * (uint32_t)j, x * m[j] + y * m[4 + j] + z * m[8 + j] + m[12 + j]);
    set_eax(c, out);
}

void X_D3DXVec3TransformNormal(X86 *c) {
    uint32_t out = arg(c, 0), v = arg(c, 1), pm = arg(c, 2);
    float m[16], x = rf(v), y = rf(v + 4), z = rf(v + 8);
    read_matrix(pm, m);
    for (int j = 0; j < 3; ++j)
        wf(out + 4u * (uint32_t)j, x * m[j] + y * m[4 + j] + z * m[8 + j]);
    set_eax(c, out);
}

void X_D3DXVec3TransformCoordArray(X86 *c) {
    uint32_t out = arg(c, 0), out_stride = arg(c, 1), in = arg(c, 2), in_stride = arg(c, 3);
    uint32_t pm = arg(c, 4), n = arg(c, 5);
    float m[16];
    read_matrix(pm, m);
    for (uint32_t k = 0; k < n; ++k) {
        uint32_t s = in + k * in_stride, d = out + k * out_stride;
        float x = rf(s), y = rf(s + 4), z = rf(s + 8);
        float r[4];
        for (int j = 0; j < 4; ++j)
            r[j] = x * m[j] + y * m[4 + j] + z * m[8 + j] + m[12 + j];
        float w = r[3] == 0.0f ? 1.0f : 1.0f / r[3];
        wf(d, r[0] * w);
        wf(d + 4, r[1] * w);
        wf(d + 8, r[2] * w);
    }
    set_eax(c, out);
}

void X_D3DXVec3Normalize(X86 *c) {
    uint32_t out = arg(c, 0), v = arg(c, 1);
    float x = rf(v), y = rf(v + 4), z = rf(v + 8);
    float len = sqrtf(x * x + y * y + z * z);
    float s = len > 0.0f ? 1.0f / len : 0.0f;
    wf(out, x * s);
    wf(out + 4, y * s);
    wf(out + 8, z * s);
    set_eax(c, out);
}

static const uint32_t D3DERR_INVALIDCALLX = 0x8876086cu;

// ---- The effect's resource, read from the module the game named -----------
static bool name_matches(uint32_t entry_name, const std::string &want) {
    uint16_t len = (uint16_t)(gm_ptr(entry_name)[0] | gm_ptr(entry_name)[1] << 8);
    if (len != want.size())
        return false;
    for (uint16_t i = 0; i < len; ++i) {
        uint16_t ch = (uint16_t)(gm_ptr(entry_name + 2 + 2u * i)[0] |
                                 gm_ptr(entry_name + 2 + 2u * i)[1] << 8);
        if (ch > 0x7f || std::toupper(ch) != std::toupper((unsigned char)want[i]))
            return false;
    }
    return true;
}

// Walks the module's resource directory for RT_RCDATA with the given name or
// number; the loader maps the headers and every section, so this is all guest
// memory.
static bool find_rcdata(uint32_t module, uint32_t name_arg, uint32_t *addr, uint32_t *size) {
    if (!module || !gm_fits(module, 0x40) || rd32(module) % 65536 != 0x5a4d)
        return false;
    uint32_t pe = module + rd32(module + 0x3c);
    if (!gm_fits(pe, 0xa0) || rd32(pe) != 0x4550)
        return false;
    uint32_t root = module + rd32(pe + 0x18 + 0x60 + 2 * 8);
    std::string want;
    uint32_t want_id = 0;
    if (name_arg > 0xffff)
        want = gm_str(name_arg, 256);
    else
        want_id = name_arg;
    auto find = [&](uint32_t dir, bool by_type, uint32_t type) -> uint32_t {
        uint16_t named = (uint16_t)(rd32(dir + 12) & 0xffff),
                 ids = (uint16_t)(rd32(dir + 12) >> 16);
        for (uint32_t i = 0; i < (uint32_t)named + ids; ++i) {
            uint32_t e = dir + 16 + 8 * i, nm = rd32(e), off = rd32(e + 4);
            bool hit;
            if (by_type)
                hit = !(nm & 0x80000000u) && nm == type;
            else if (!want.empty())
                hit = (nm & 0x80000000u) && name_matches(root + (nm & 0x7fffffffu), want);
            else
                hit = !(nm & 0x80000000u) && nm == want_id;
            if (hit)
                return off;
        }
        return 0;
    };
    uint32_t type_off = find(root, true, 10); // RT_RCDATA
    if (!(type_off & 0x80000000u))
        return false;
    uint32_t name_off = find(root + (type_off & 0x7fffffffu), false, 0);
    if (!(name_off & 0x80000000u))
        return false;
    uint32_t lang_dir = root + (name_off & 0x7fffffffu);
    uint32_t data_entry = root + (rd32(lang_dir + 16 + 4) & 0x7fffffffu);
    *addr = module + rd32(data_entry);
    *size = rd32(data_entry + 4);
    return gm_fits(*addr, *size);
}

// (pDevice, hSrcModule, pSrcResource, pDefines, pInclude, Flags, pPool,
//  ppEffect, ppCompilationErrors)
void X_D3DXCreateEffectFromResourceA(X86 *c) {
    uint32_t device = arg(c, 0), module = arg(c, 1), resource = arg(c, 2);
    uint32_t flags = arg(c, 5), pool = arg(c, 6), out = arg(c, 7), errors = arg(c, 8);
    if (errors && gm_valid(errors, 4))
        wr32(errors, 0);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, D3DERR_INVALIDCALLX);
        return;
    }
    wr32(out, 0);
    ComObj *dev = com_this(device);
    if (!dev || dev->kind != K_D3D9DEVICE || !dev->d3d9_device) {
        com_ret(c, D3DERR_INVALIDCALLX);
        return;
    }
    if (!module)
        module = IMAGE_BASE;
    uint32_t addr = 0, size = 0;
    if (!find_rcdata(module, resource, &addr, &size)) {
        LOGW("d3dx9: effect resource %08x: no RCDATA resource of that name in module %08x",
             resource, module);
        com_ret(c, D3DERR_INVALIDCALLX);
        return;
    }
    efx_create(c, dev, pool, flags, out, (const uint8_t *)gm_ptr(addr), size);
}

static const ImportShim g_d3dx9_27_exports[] = {
    {"d3dx9_27.dll", "D3DXCheckVersion", 2, X_D3DXCheckVersion},
    {"d3dx9_27.dll", "D3DXCompileShader", 10, X_D3DXCompileShader},
    {"d3dx9_27.dll", "D3DXGetPixelShaderProfile", 1, X_D3DXGetPixelShaderProfile},
    {"d3dx9_27.dll", "D3DXGetVertexShaderProfile", 1, X_D3DXGetVertexShaderProfile},
    {"d3dx9_27.dll", "D3DXMatrixMultiply", 3, X_D3DX33MatrixMultiply},
    {"d3dx9_27.dll", "D3DXMatrixMultiplyTranspose", 3, X_D3DXMatrixMultiplyTranspose},
    {"d3dx9_27.dll", "D3DXMatrixTranspose", 2, X_D3DX33MatrixTranspose},
    {"d3dx9_27.dll", "D3DXVec3Transform", 3, X_D3DXVec3Transform},
    {"d3dx9_27.dll", "D3DXVec3TransformNormal", 3, X_D3DXVec3TransformNormal},
};

static const ImportShim g_d3dx9_exports[] = {
    {"d3dx9_26.dll", "D3DXCreateEffectPool", 1, X_D3DX33CreateEffectPool},
    {"d3dx9_26.dll", "D3DXCreateEffectFromResourceA", 9, X_D3DXCreateEffectFromResourceA},
    {"d3dx9_26.dll", "D3DXMatrixMultiply", 3, X_D3DXMatrixMultiply},
    {"d3dx9_26.dll", "D3DXMatrixInverse", 3, X_D3DXMatrixInverse},
    {"d3dx9_26.dll", "D3DXMatrixTranspose", 2, X_D3DXMatrixTranspose},
    {"d3dx9_26.dll", "D3DXMatrixOrthoLH", 5, X_D3DXMatrixOrthoLH},
    {"d3dx9_26.dll", "D3DXMatrixPerspectiveLH", 5, X_D3DXMatrixPerspectiveLH},
    {"d3dx9_26.dll", "D3DXMatrixTranslation", 4, X_D3DXMatrixTranslation},
    {"d3dx9_26.dll", "D3DXVec4Transform", 3, X_D3DXVec4Transform},
    {"d3dx9_26.dll", "D3DXVec3Transform", 3, X_D3DXVec3Transform},
    {"d3dx9_26.dll", "D3DXVec3TransformNormal", 3, X_D3DXVec3TransformNormal},
    {"d3dx9_26.dll", "D3DXVec3TransformCoordArray", 6, X_D3DXVec3TransformCoordArray},
    {"d3dx9_26.dll", "D3DXVec3Normalize", 2, X_D3DXVec3Normalize},
};

static const ImportShim g_d3dx9_30_exports[] = {
    {"d3dx9_30.dll", "D3DXVec3Transform", 3, X_D3DXVec3Transform},
    {"d3dx9_30.dll", "D3DXMatrixInverse", 3, X_D3DX33MatrixInverse},
    {"d3dx9_30.dll", "D3DXCreateVolumeTextureFromFileInMemory", 4,
     X_D3DXCreateVolumeTextureFromFileInMemory},
    {"d3dx9_30.dll", "D3DXCreateCubeTextureFromFileInMemory", 4,
     X_D3DXCreateCubeTextureFromFileInMemory},
    {"d3dx9_30.dll", "D3DXLoadSurfaceFromSurface", 8, X_D3DXLoadSurfaceFromSurface},
    {"d3dx9_30.dll", "D3DXSaveSurfaceToFileA", 5, X_D3DXSaveSurfaceToFileA},
    {"d3dx9_30.dll", "D3DXCreateEffectPool", 1, X_D3DX33CreateEffectPool},
    {"d3dx9_30.dll", "D3DXCreateEffectFromResourceA", 9, X_D3DXCreateEffectFromResourceA},
    {"d3dx9_30.dll", "D3DXVec4Transform", 3, X_D3DXVec4Transform},
    {"d3dx9_30.dll", "D3DXMatrixPerspectiveLH", 5, X_D3DXMatrixPerspectiveLH},
    {"d3dx9_30.dll", "D3DXMatrixTranslation", 4, X_D3DXMatrixTranslation},
    {"d3dx9_30.dll", "D3DXVec3TransformCoordArray", 6, X_D3DXVec3TransformCoordArray},
    {"d3dx9_30.dll", "D3DXVec3Normalize", 2, X_D3DXVec3Normalize},
    {"d3dx9_30.dll", "D3DXVec3TransformNormal", 3, X_D3DXVec3TransformNormal},
    {"d3dx9_30.dll", "D3DXSaveSurfaceToFileInMemory", 5, nullptr},
    {"d3dx9_30.dll", "D3DXFillVolumeTexture", 3, X_D3DXFillVolumeTexture},
    {"d3dx9_30.dll", "D3DXMatrixMultiply", 3, X_D3DX33MatrixMultiply},
    {"d3dx9_30.dll", "D3DXMatrixOrthoLH", 5, X_D3DXMatrixOrthoLH},
    {"d3dx9_30.dll", "D3DXMatrixTranspose", 2, X_D3DX33MatrixTranspose},
};

#include "d3dx9_33_interfaces.inc"
#include "d3dx9_imports.inc"

void d3dx9_reset() {
    d3dx_shader_helpers_reset();
}

void d3dx9_register() {
    static bool done = false;
    if (done)
        return;
    done = true;
    com_define(IF_D3DXEFFECT33, "d3dx9_33.dll", "ID3DXEffect", g_effect_33, std::size(g_effect_33));
    com_define(IF_D3DXEFFECTPOOL33, "d3dx9_33.dll", "ID3DXEffectPool", g_effect_pool_33,
               std::size(g_effect_pool_33));
    com_define(IF_D3DXBUFFER, "d3dx9_33.dll", "ID3DXBuffer", g_buffer_33, std::size(g_buffer_33));
    com_define(IF_D3DXCONSTANTTABLE, "d3dx9_33.dll", "ID3DXConstantTable", g_constant_table_33,
               std::size(g_constant_table_33));
    com_define(IF_D3DXEFFECTCOMPILER, "d3dx9_33.dll", "ID3DXEffectCompiler", g_effect_compiler_33,
               std::size(g_effect_compiler_33));
    com_register_iid(IF_D3DXEFFECT33, IID_ID3DXEffect_33);
    com_register_iid(IF_D3DXEFFECTPOOL33, IID_ID3DXEffectPool_33);
    com_register_iid(IF_D3DXBUFFER, IID_ID3DXBuffer_33);
    com_register_iid(IF_D3DXCONSTANTTABLE, IID_ID3DXConstantTable_33);
    com_register_iid(IF_D3DXEFFECTCOMPILER, IID_ID3DXEffectCompiler_33);
    com_bind(IF_D3DXBUFFER, K_D3DXCONSTANTTABLE);
    com_bind(IF_D3DXCONSTANTTABLE, K_D3DXCONSTANTTABLE);
    com_bind(IF_D3DXEFFECT33, K_D3DXEFFECT);
    com_bind(IF_D3DXEFFECTPOOL33, K_D3DXEFFECTPOOL);
    com_set_destructor(K_D3DXEFFECT, d3dx_effect_destroy);
    com_set_destructor(K_D3DXEFFECTPOOL, d3dx_effect_pool_destroy);
    com_set_destructor(K_D3DXCONSTANTTABLE, d3dx_ctab_destroy);
    imports_register(g_d3dx9_exports, std::size(g_d3dx9_exports));
    imports_register(g_d3dx9_27_exports, std::size(g_d3dx9_27_exports));
    imports_register(g_d3dx9_30_exports, std::size(g_d3dx9_30_exports));
    imports_register(g_d3dx9_33_exports, std::size(g_d3dx9_33_exports));
}
