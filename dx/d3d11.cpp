#include "d3d11.h"
#include "../runtime/imports.h"

void d3d11_register() {
    static const ImportShim shims[] = {
        {"d3d11.dll", "D3D11CreateDeviceAndSwapChain", 12, imports_unsupported},
        {"d3d11.dll", "D3D11CreateDevice", 10, imports_unsupported},
    };
    imports_register(shims, std::size(shims));
}
