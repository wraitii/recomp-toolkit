#include "d3d11.h"
#include "../runtime/imports.h"

void dxgi_register() {
    static const ImportShim shims[] = {
        {"dxgi.dll", "CreateDXGIFactory", 2, imports_unsupported},
        {"dxgi.dll", "CreateDXGIFactory1", 2, imports_unsupported},
        {"dxgi.dll", "CreateDXGIFactory2", 3, imports_unsupported},
    };
    imports_register(shims, std::size(shims));
}
