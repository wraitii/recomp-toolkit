#include "d3d11.h"
#include "../runtime/imports.h"

void d3dcompiler_register() {
    static const ImportShim shims[] = {
        {"d3dcompiler_47.dll", "D3DCompile", 11, imports_unsupported},
    };
    imports_register(shims, std::size(shims));
}
