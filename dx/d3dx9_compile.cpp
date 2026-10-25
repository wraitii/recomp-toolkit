#include "com.h"
#include "../runtime/memory.h"
#include "d3dx9shader_shim.h"
#ifndef RECOMP_NULL_HOST
#include <vkd3d_shader.h>
#endif
#include <cstring>
#include <cstdint>
#include <limits>
#include <string>

uint32_t d3dx_make_buffer(const void *data, uint32_t size) {
    uint32_t guest_data = heap_alloc(size ? size : 1, true, 16);
    if (!guest_data)
        return 0;
    if (size)
        std::memcpy(gm_ptr(guest_data), data, size);
    ComObj *object = com_new(K_D3DXCONSTANTTABLE);
    if (!object) {
        heap_free(guest_data);
        return 0;
    }
    object->d3dx_ctab_buffer = guest_data;
    object->d3dx_ctab_size = size;
    uint32_t view = com_view(object, IF_D3DXBUFFER);
    if (!view)
        com_release(object);
    return view;
}

#ifdef RECOMP_NULL_HOST
void X_D3DXCompileShader(X86 *c) {
    LOGW("D3DXCompileShader: unavailable on the null host");
    com_ret(c, 0x80004001);
}
#else
namespace {
constexpr uint32_t kEFail = 0x80004005;
constexpr uint32_t kEInvalidArg = 0x80070057;
constexpr uint32_t kEOutOfMemory = 0x8007000e;
constexpr uint32_t kEPointer = 0x80004003;
constexpr uint32_t kSkipValidation = 0x2;
constexpr uint32_t kPackMatrixRowMajor = 0x8;
constexpr uint32_t kPackMatrixColumnMajor = 0x10;
constexpr uint32_t kPartialPrecision = 0x20;
constexpr uint32_t kD3dxCompileFlags =
    kSkipValidation | kPackMatrixRowMajor | kPackMatrixColumnMajor | kPartialPrecision;

bool guest_string(uint32_t address, size_t limit, std::string &value) {
    if (!address)
        return false;
    value.clear();
    for (size_t i = 0; i < limit; ++i) {
        uint64_t current = uint64_t(address) + i;
        if (current >= GUEST_SIZE)
            return false;
        char c = char(rd8(uint32_t(current)));
        if (!c)
            return true;
        value.push_back(c);
    }
    return false;
}

void error_buffer(uint32_t output, const char *message) {
    if (!output || !message)
        return;
    size_t length = std::strlen(message);
    if (length >= std::numeric_limits<uint32_t>::max())
        return;
    uint32_t size = uint32_t(length + 1);
    uint32_t view = d3dx_make_buffer(message, size);
    if (view)
        wr32(output, view);
}

void fail(X86 *c, uint32_t result, uint32_t errors, const char *message) {
    error_buffer(errors, message);
    com_ret(c, result);
}
} // namespace

void X_D3DXCompileShader(X86 *c) {
    uint32_t shader_output = arg(c, 7), errors_output = arg(c, 8), table_output = arg(c, 9);
    if (shader_output && gm_valid(shader_output, 4))
        wr32(shader_output, 0);
    if (errors_output && gm_valid(errors_output, 4))
        wr32(errors_output, 0);
    if (table_output && gm_valid(table_output, 4))
        wr32(table_output, 0);
    if (!shader_output || !gm_valid(shader_output, 4)) {
        com_ret(c, kEPointer);
        return;
    }
    if ((errors_output && !gm_valid(errors_output, 4)) ||
        (table_output && !gm_valid(table_output, 4))) {
        fail(c, kEInvalidArg, 0, "D3DXCompileShader: invalid optional output pointer");
        return;
    }
    uint32_t source = arg(c, 0), source_size = arg(c, 1), flags = arg(c, 6);
    if (arg(c, 2) || arg(c, 3)) {
        LOGW("D3DXCompileShader: non-null macro definitions or include handler are unsupported");
        fail(c, kEInvalidArg, errors_output,
             "D3DXCompileShader: macro definitions and include handlers are unsupported");
        return;
    }
    if (table_output) {
        LOGW("D3DXCompileShader: non-null constant-table output is unsupported");
        fail(c, kEInvalidArg, errors_output,
             "D3DXCompileShader: constant-table output is unsupported");
        return;
    }
    if ((flags & ~kD3dxCompileFlags) || !source || !source_size || !gm_valid(source, source_size)) {
        LOGW("D3DXCompileShader: unsupported flags %08x or invalid source span", flags);
        fail(c, kEInvalidArg, errors_output, "D3DXCompileShader: invalid flags or source span");
        return;
    }
    std::string entry, profile;
    if (!guest_string(arg(c, 4), 128, entry) || !guest_string(arg(c, 5), 32, profile)) {
        fail(c, kEInvalidArg, errors_output,
             "D3DXCompileShader: invalid entry point or profile string");
        return;
    }
    if (flags & kSkipValidation)
        LOGV("D3DXCompileShader: vkd3d-shader still parses and validates HLSL structure");
    if (flags & kPartialPrecision)
        LOGV("D3DXCompileShader: partial precision accepted; vkd3d-shader emits full precision "
             "bytecode");

    vkd3d_shader_compile_option options[2]{};
    options[0].name = VKD3D_SHADER_COMPILE_OPTION_API_VERSION;
    options[0].value = VKD3D_SHADER_API_VERSION_2_1;
    options[1].name = VKD3D_SHADER_COMPILE_OPTION_PACK_MATRIX_ORDER;
    options[1].value = (flags & kPackMatrixRowMajor)
                           ? VKD3D_SHADER_COMPILE_OPTION_PACK_MATRIX_ROW_MAJOR
                           : VKD3D_SHADER_COMPILE_OPTION_PACK_MATRIX_COLUMN_MAJOR;
    vkd3d_shader_hlsl_source_info hlsl{};
    hlsl.type = VKD3D_SHADER_STRUCTURE_TYPE_HLSL_SOURCE_INFO;
    hlsl.entry_point = entry.c_str();
    hlsl.profile = profile.c_str();
    vkd3d_shader_compile_info info{};
    info.type = VKD3D_SHADER_STRUCTURE_TYPE_COMPILE_INFO;
    info.next = &hlsl;
    info.source.code = gm_ptr(source);
    info.source.size = source_size;
    info.source_type = VKD3D_SHADER_SOURCE_HLSL;
    info.target_type = VKD3D_SHADER_TARGET_D3D_BYTECODE;
    info.options = options;
    info.option_count = 2;
    info.log_level = VKD3D_SHADER_LOG_WARNING;
    vkd3d_shader_code bytecode{};
    char *messages = nullptr;
    int result = vkd3d_shader_compile(&info, &bytecode, &messages);
    if (result < 0) {
        const char *message =
            messages && *messages ? messages : "D3DXCompileShader: vkd3d-shader compilation failed";
        LOGW("D3DXCompileShader: %s", message);
        fail(c, kEFail, errors_output, message);
        vkd3d_shader_free_shader_code(&bytecode);
        vkd3d_shader_free_messages(messages);
        return;
    }
    if (!bytecode.code || !bytecode.size || bytecode.size > std::numeric_limits<uint32_t>::max()) {
        LOGW("D3DXCompileShader: vkd3d-shader returned invalid bytecode size %zu", bytecode.size);
        fail(c, kEFail, errors_output, "D3DXCompileShader: compiler returned invalid bytecode");
        vkd3d_shader_free_shader_code(&bytecode);
        vkd3d_shader_free_messages(messages);
        return;
    }
    uint32_t shader_view = d3dx_make_buffer(bytecode.code, uint32_t(bytecode.size));
    if (!shader_view) {
        fail(c, kEOutOfMemory, errors_output,
             "D3DXCompileShader: could not allocate shader buffer");
        vkd3d_shader_free_shader_code(&bytecode);
        vkd3d_shader_free_messages(messages);
        return;
    }
    if (messages && *messages) {
        error_buffer(errors_output, messages);
        LOGV("D3DXCompileShader: %s", messages);
    }
    wr32(shader_output, shader_view);
    com_ret(c, 0);
    vkd3d_shader_free_shader_code(&bytecode);
    vkd3d_shader_free_messages(messages);
}
#endif
