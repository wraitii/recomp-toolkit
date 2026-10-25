# D3D9 interpreter MVP

The opt-in interpreter implements a bounded SM2 VS/PS family; Spore rendering,
native Metal compilation and GPU performance still require user-run validation.
`src/shader/uber/` defines ABI v4 / interpreter v8. A program is 5136 bytes:
a four-u32 header (ABI version, pixel flag, instruction count, definition count),
64 zero-padded 16-u32 instruction records and 32 definition records (index/padding
and float bits). Opcode bits 8–10 mark immediate sources and bit 16 selects projected TEX; source records
hold type/index/swizzle/flags. Flag bit 0 negates, bit 8 selects relative addressing,
and bits 9–10 select the address component. Original bytecode equality plus ABI
version identifies program data; capacity overflow is excluded.

Admitted arithmetic is MOV/ADD/MUL/MAD/DP3/DP4/RCP/RSQ/MIN/MAX/SLT/LRP/CMP/
NRM/FRC, plus vertex MOVA and ordinary/projected pixel TEX through 2D s0.
Capacities are eight temporaries, 64 instructions, 32 float DEFs, 256/32 float
constants, vertex oPos/oD0–1/oT0–2/a0, pixel v0–1/t0–2 and oC0. Up to five
consecutive vertex inputs admit position, normal, color0/1, UV0 and blend
indices/weights. Source swizzles, negation and destination
masks are supported. MOVA rounds using the existing specializer's nearest-integer
helper. Relative vertex constants use the selected a0 component and the same
clamped index as specialization; shader DEFs override device constants even for
relative reads. Direct DEF reads become immediate bits. TEX requires unmodified,
identity-swizzled t0–2 or temporary coordinates and s0, a temporary destination and
full mask. Pixel partial-precision hints on instructions/declarations execute
at full precision, as permitted by [D3D9](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/dx9-graphics-reference-asm-ps-instructions-modifiers-ps-2-0) and matching specialization.
Integer/bool definitions, other modifiers, shifts, pixel relative
addressing, flow control, coissue and other models/registers/samplers remain
excluded. TEXKILL accepts full-mask temporary or t0–2 operands and records any negative
component (all four for SM2, matching specialization and vkd3d). Its destination
record marks no write; its operand is encoded as an identity-swizzled source.
Specialization remains unchanged.

All families use five Float32x4 attributes (80-byte stride), existing 42-component
varyings, smooth interpolation, CW triangles and one sample. Shorter declarations
pad unused attributes with zero; lowering rejects reads of undeclared inputs.
`compare-family11` prepares the original family; hybrid and interpreter-only
prepare 14 exact captured states, including all five previous families. They use
BGRA8/RGBA8, triangle list/strip, no culling/front culling, opaque/alpha/additive
blending and absent/LESS/LEQUAL/ALWAYS depth, with writes only where captured.
The finite manifest is in `src/render/interpreter.rs`; other combinations fall back.

All share one module, layout and program allocations. Existing input conversion,
half-pixel correction, raster output processing and sampler state are reused;
opaque texture alpha is a per-draw flag. Bound textures must be filterable ordinary
2D resources. Texture-free shaders bind a private unused texture/sampler to the
shared layout. Integer/bool banks, vertex textures, biased sampling,
shader depth output and MRT have no coverage. Stencil writes remain disabled.

One bind group contains four dynamic uniforms (bindings 0/1: 4096-byte VS bank /
272-byte parameters; 2/3: 512-byte PS bank / 64-byte parameters), texture/sampler
4/20, and immutable 5136-byte program uniforms 36/37. It needs three uniforms per
stage, four dynamic uniforms, five vertex attributes, one texture/sampler and
42 inter-stage components; initialization checks the requested device limits.
Prepared module/pipeline ownership follows the device. Queued draws and bindings
retain their program buffers; reset clears program uploads and replacement devices
prepare the selected manifest. Specialization cache eviction retains interpreter residency.

Both stages execute a bounded loop through uniform instruction data, with a
draw-uniform instruction index and a maximum of 64 admitted instructions. Guest
flow control remains excluded. TEXKILL accumulates a sticky kill flag at its
instruction position; discard occurs after all instruction slots, preserving
implicit sampling derivatives. Sampling remains implicit; alpha discard occurs
after instruction execution. The build validates the generated WGSL with all Naga
30 validation flags, verifies uploaded ABI layouts and translates both entry
points to Metal with real binding maps. No derivative diagnostic is suppressed
and no fixed LOD is substituted. Native compiler acceptance, rendering agreement,
gameplay agreement and execution cost remain capture gates.
Semantic references: Microsoft's [MOVA](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/mova---vs),
[FRC](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/frc---vs), [DP4](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/dp4---vs),
[TEXLD](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/texld---ps-2-0),
[TEXLDP](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/texldp---ps),
[RCP](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/rcp---vs) and
[RSQ](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/rsq---ps)
and [WGSL uniformity](https://www.w3.org/TR/WGSL/#uniformity).

# D3D8 shader-model 1.1

The guest bridge implements Create/Set/Get/DeleteVertexShader and PixelShader,
GetVertexShaderDeclaration/Function and GetPixelShaderFunction. It copies guest
bytecode and declarations into device-owned storage. Handles are above
`0xf0000000`, distinct from FVF values and shader version tokens. Queries return
required byte counts and `D3DERR_MOREDATA` for short buffers. Invalid or deleted
handles return `D3DERR_INVALIDCALL`. Reset clears bindings and device constants
and retains shader objects; deleting a bound shader restores its fixed stage.

`src/d3d8/shader.rs` decodes tokens and emits WGSL. Vertex and fragment stages
can be selected independently, alongside the existing fixed-function stages.
Pipelines/modules are cached by program identities and vertex layout. Draws
retain pipelines, texture views/samplers and copies of both constant banks and
bump state, preserving queued work across later state changes and deletion.

## Supported slice

- `vs.1.0/1.1`, `ps.1.0/1.1`; other versions fail during creation by name.
- Creation validates and retains consecutive TEXM3X3PAD/TEXM3X3PAD/TEXM3X3VSPEC
  sequences, including their `_bx2` source modifier. These programs remain
  queryable and bindable; drawing with them fails by name because cube/volume
  sampling and four-component texture-coordinate varyings are not implemented.
  Cube/volume capabilities remain unadvertised. This permits applications to
  create their shader libraries before selecting a rendering path.
  `shader-corpus --d3d8-create <shader.bin>...` checks creation from copied
  bytecode and reports draw availability separately; it does not validate GPU execution.
- Stream-0 declarations: FLOAT1/2/3/4, D3DCOLOR, UBYTE4, SHORT2/4, SKIP and
  CONST. Float inputs extend with zero components and w=1. Packed D3DCOLOR is
  BGRA bytes converted to floating RGBA. Declaration-only shaders support the
  existing packed fixed-function FVFs; other layouts fail by name.
- 96 vertex and eight pixel float4 constants. Declaration CONST and shader DEF
  are local to the program, independent of the Set/Get device banks.
- MOV, ADD, SUB, MAD, MUL, RCP, RSQ, DP3/4, MIN/MAX, SLT/SGE, EXP/LOG,
  EXPP/LOGP, LIT, DST, LRP, FRC, M4x4/M4x3/M3x4/M3x3/M3x2, CND, SINCOS and DEF.
  Stage-specific instructions are rejected where not implemented. Source
  swizzles, negation, bias, signed scale (`_bx2`), complement, destination
  masks, saturation and shift modifiers are decoded. Temporaries preserve
  aliasing across partial destination writes. Relative vertex constant indexing
  uses a0.x; `DIVERGENCE(original):` indices outside the bank clamp to its bounds (original behavior
  outside the valid constant range is unverified).
- oPos, oFog, oD0/1 and oT0..7 vertex registers; four 2D texture coordinates
  reach the fragment shader. TEXCOORD, TEXKILL, TEX, TEXBEM/TEXBEML and
  TEXREG2AR/GB use four retained texture/sampler bindings. TEXBEM uses its
  **destination** stage's BUMPENVMAT00/01/10/11 state; TEXBEML also uses that
  stage's luminance scale/offset. Combiner and fixed vertex texgen/transform
  state do not affect programmable stages. Sampler filters/addressing/mip
  limits/LOD bias still apply. V8U8 bump textures retain signed U,V bytes
  through guest locks and mip uploads and sample as Rg8Snorm (B=0, A=1).
  Alpha test and fog remain raster operations.

The renderer advertises vs.1.1 / ps.1.1, 96 vertex constants,
MaxPixelShaderValue=1.0 and four simultaneous textures. Its fixed-function
combiner capacity remains two stages. These are bounded implementation caps,
not a claim of complete shader-model conformance.

Unsupported features include instruction coissue, texture matrix instruction
families, point-size outputs, multistream/tessellator declarations, shader
versions beyond 1.1, cube/volume textures and signed bump texture formats
other than V8U8. A fixed-function VS combined with a PS currently requires an existing
textured FVF and no texture transforms or generated texture coordinates. These
combinations fail by name. Full original GPU precision and
out-of-range arithmetic equivalence are unverified. Fixed-function D3D7 uses
the same renderer but gains no new bump combiner capability in this change.

## Reference and validation

The token/declaration definitions are the unchanged Wine 11.0 headers in
[third_party/wine-d3d8](../../third_party/wine-d3d8/README.md). Semantic evidence
uses that same commit's [D3D8 device](https://github.com/wine-mirror/wine/blob/db11d0fe6a169c457e23d007e20404643d067aa8/dlls/d3d8/device.c),
[declaration constants](https://github.com/wine-mirror/wine/blob/db11d0fe6a169c457e23d007e20404643d067aa8/dlls/d3d8/vertexdeclaration.c),
[SM1 decoder](https://github.com/wine-mirror/wine/blob/db11d0fe6a169c457e23d007e20404643d067aa8/dlls/wined3d/shader_sm1.c)
and [GLSL operations](https://github.com/wine-mirror/wine/blob/db11d0fe6a169c457e23d007e20404643d067aa8/dlls/wined3d/glsl_shader.c).
The WGSL emitter is first-party code; Wine source is semantic reference.

From the game repository, using its Python environment:

```sh
tools/.venv/bin/python recomp-kit/tools/test.py --game-dir "$PWD" --d3d8-wgpu test
tools/.venv/bin/python recomp-kit/tools/test.py --game-dir "$PWD" --d3d8-wgpu probe
```

The Rust suite validates composed stages with naga, rejects unsupported and
truncated tokens, checks declaration layout and embedded END bit patterns, and
performs GPU readback for independent queued pixel constant changes,
declaration-local constants and a dependent stage-3 texture read with an
off-diagonal bump matrix, independent fixed-function vertex/pixel shader
selection and padded stream-stride pipeline caching. Signed V8U8 tests cover positive,
negative and zero offsets in both channels, mip selection and texture re-upload.
Guest COM fixture tests
cover shader handle ownership, binding/deletion, bytecode/declaration size
queries and short-buffer behavior without a GPU. The existing headless probe checks fixed-function
rendering after the binding-layout extension. These tests require GPU access;
they do not establish original-D3D8 equivalence or game playability.

`RECOMP_D3D8_TRACE_SHADERS=1` prints creation handles, versions and token counts.
No original shader source or bytecode fixtures are committed.

`RECOMP_D3D8_TRACE_SHADER_DRAWS=<n>` reports up to n programmable draws,
including all four bindings, the constant banks, bump/luminance state and
sample vertices. `RECOMP_D3D8_TRACE_SHADER_STAGE2=1` restricts captures to
programs sampling stage 2. `RECOMP_D3D8_TRACE_SHADER_DIR=<dir>` saves rendered
inputs from GPU readback; CPU texture dumps alone cannot show a rendered
reflection. Capturing these inputs forces submissions/readback and is for
diagnosis only, not performance measurement.
