#pragma once

#include "com.h"

struct X86;

enum {
    kD3dxSetValue = 0,
    kD3dxSetBool = 1,
    kD3dxSetBoolArray = 2,
    kD3dxSetInt = 3,
    kD3dxSetIntArray = 4,
    kD3dxSetFloat = 5,
    kD3dxSetFloatArray = 6,
    kD3dxSetVector = 7,
    kD3dxSetVectorArray = 8,
    kD3dxSetMatrix = 9,
    kD3dxSetMatrixArray = 10,
    kD3dxSetMatrixPointerArray = 11,
    kD3dxSetMatrixTranspose = 12,
    kD3dxSetMatrixTransposeArray = 13,
    kD3dxSetMatrixTransposePointerArray = 14,
};

void D3DXBuffer_GetBufferPointer(X86 *c);
void D3DXBuffer_GetBufferSize(X86 *c);
uint32_t d3dx_make_buffer(const void *data, uint32_t size);
void X_D3DXCompileShader(X86 *c);

void D3DXConstantTable_GetDesc(X86 *c);
void D3DXConstantTable_GetConstantDesc(X86 *c);
void D3DXConstantTable_GetSamplerIndex(X86 *c);
void D3DXConstantTable_GetConstant(X86 *c);
void D3DXConstantTable_GetConstantByName(X86 *c);
void D3DXConstantTable_GetConstantElement(X86 *c);
void D3DXConstantTable_SetDefaults(X86 *c);
void D3DXConstantTable_SetValue(X86 *c);
void D3DXConstantTable_SetBool(X86 *c);
void D3DXConstantTable_SetBoolArray(X86 *c);
void D3DXConstantTable_SetInt(X86 *c);
void D3DXConstantTable_SetIntArray(X86 *c);
void D3DXConstantTable_SetFloat(X86 *c);
void D3DXConstantTable_SetFloatArray(X86 *c);
void D3DXConstantTable_SetVector(X86 *c);
void D3DXConstantTable_SetVectorArray(X86 *c);
void D3DXConstantTable_SetMatrix(X86 *c);
void D3DXConstantTable_SetMatrixArray(X86 *c);
void D3DXConstantTable_SetMatrixPointerArray(X86 *c);
void D3DXConstantTable_SetMatrixTranspose(X86 *c);
void D3DXConstantTable_SetMatrixTransposeArray(X86 *c);
void D3DXConstantTable_SetMatrixTransposePointerArray(X86 *c);

void d3dx_ctab_destroy(ComObj *o);
void d3dx_shader_helpers_reset();

void X_D3DXGetShaderVersion(X86 *c);
void X_D3DXGetShaderInputSemantics(X86 *c);
void X_D3DXGetShaderConstantTable(X86 *c);
void X_D3DXGetVertexShaderProfile(X86 *c);
void X_D3DXGetPixelShaderProfile(X86 *c);
void X_D3DX33MatrixMultiply(X86 *c);
void X_D3DX33MatrixTranspose(X86 *c);
void X_D3DX33MatrixInverse(X86 *c);
