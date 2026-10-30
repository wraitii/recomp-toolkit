if(RECOMP_DEPENDENCIES)
  set(VKD3D_STAGED "${RECOMP_DEPENDENCIES}/vkd3d")
  foreach(VKD3D_FILE libvkd3d_shader.a include/vkd3d_shader.h include/vkd3d_types.h)
    if(NOT EXISTS "${VKD3D_STAGED}/${VKD3D_FILE}")
      message(FATAL_ERROR "Staged shader compiler input is missing: ${VKD3D_STAGED}/${VKD3D_FILE}")
    endif()
  endforeach()
  find_package(Threads REQUIRED)
  add_library(vkd3d_shader STATIC IMPORTED GLOBAL)
  set_target_properties(vkd3d_shader PROPERTIES
    IMPORTED_LOCATION "${VKD3D_STAGED}/libvkd3d_shader.a"
    INTERFACE_INCLUDE_DIRECTORIES "${VKD3D_STAGED}/include"
    INTERFACE_COMPILE_DEFINITIONS LIBVKD3D_SHADER_SOURCE
    INTERFACE_LINK_LIBRARIES Threads::Threads)
  return()
endif()

include(FetchContent)
include(CheckIncludeFile)
include(CheckSymbolExists)

FetchContent_Declare(vkd3d_shader_source
  URL https://dl.winehq.org/vkd3d/source/vkd3d-2.1.tar.xz
  URL_HASH SHA256=7510146aff2adfb4ae07ab890701a607e5ff7c66e57100cfc9f630ec92eeda6a)
FetchContent_Declare(spirv_headers
  URL https://codeload.github.com/KhronosGroup/SPIRV-Headers/tar.gz/refs/tags/vulkan-sdk-1.3.268.0
  URL_HASH SHA256=1022379e5b920ae21ccfb5cb41e07b1c59352a18c3d3fdcbf38d6ae7733384d4)
FetchContent_MakeAvailable(vkd3d_shader_source spirv_headers)

set(VKD3D_SHADER_SRC "${vkd3d_shader_source_SOURCE_DIR}/libs/vkd3d-shader")
set(VKD3D_SHADER_INC "${vkd3d_shader_source_SOURCE_DIR}/include")
set(VKD3D_SHADER_PRIVATE "${VKD3D_SHADER_INC}/private")
set(VKD3D_SHADER_GENERATED "${CMAKE_CURRENT_BINARY_DIR}/vkd3d-shader-generated")
file(MAKE_DIRECTORY "${VKD3D_SHADER_GENERATED}/private")
file(CONFIGURE OUTPUT "${VKD3D_SHADER_GENERATED}/config.h" CONTENT
  "#define HAVE_SPIRV_UNIFIED1_SPIRV_H 1\n#define HAVE_SPIRV_UNIFIED1_GLSL_STD_450_H 1\n")
file(CONFIGURE OUTPUT "${VKD3D_SHADER_GENERATED}/private/vkd3d_version.h" CONTENT "#define VKD3D_VCS_ID \"\"\n")

find_package(FLEX REQUIRED)
if(APPLE AND NOT BISON_EXECUTABLE)
  foreach(VKD3D_PREFIX IN LISTS CMAKE_PREFIX_PATH)
    if(EXISTS "${VKD3D_PREFIX}/bin/bison")
      set(BISON_EXECUTABLE "${VKD3D_PREFIX}/bin/bison")
      break()
    endif()
  endforeach()
  if(NOT BISON_EXECUTABLE)
    foreach(VKD3D_BISON /opt/homebrew/opt/bison/bin/bison /usr/local/opt/bison/bin/bison)
      if(EXISTS "${VKD3D_BISON}")
        set(BISON_EXECUTABLE "${VKD3D_BISON}")
        break()
      endif()
    endforeach()
  endif()
endif()
find_package(BISON 3.0 REQUIRED)
find_package(Threads REQUIRED)
check_include_file(xlocale.h VKD3D_HAVE_XLOCALE_H)
set(VKD3D_SAVED_REQUIRED_DEFINITIONS "${CMAKE_REQUIRED_DEFINITIONS}")
list(APPEND CMAKE_REQUIRED_DEFINITIONS -D_GNU_SOURCE)
check_symbol_exists(strtof_l stdlib.h VKD3D_HAVE_STRTOF_L)
set(CMAKE_REQUIRED_DEFINITIONS "${VKD3D_SAVED_REQUIRED_DEFINITIONS}")
FLEX_TARGET(vkd3d_preproc "${VKD3D_SHADER_SRC}/preproc.l"
  "${VKD3D_SHADER_GENERATED}/preproc.yy.c")
BISON_TARGET(vkd3d_preproc "${VKD3D_SHADER_SRC}/preproc.y"
  "${VKD3D_SHADER_GENERATED}/preproc.tab.c"
  DEFINES_FILE "${VKD3D_SHADER_GENERATED}/preproc.tab.h")
FLEX_TARGET(vkd3d_hlsl "${VKD3D_SHADER_SRC}/hlsl.l"
  "${VKD3D_SHADER_GENERATED}/hlsl.yy.c")
BISON_TARGET(vkd3d_hlsl "${VKD3D_SHADER_SRC}/hlsl.y"
  "${VKD3D_SHADER_GENERATED}/hlsl.tab.c"
  DEFINES_FILE "${VKD3D_SHADER_GENERATED}/hlsl.tab.h")
ADD_FLEX_BISON_DEPENDENCY(vkd3d_preproc vkd3d_preproc)
ADD_FLEX_BISON_DEPENDENCY(vkd3d_hlsl vkd3d_hlsl)

add_library(vkd3d_shader STATIC
  "${vkd3d_shader_source_SOURCE_DIR}/libs/vkd3d-common/debug.c"
  "${vkd3d_shader_source_SOURCE_DIR}/libs/vkd3d-common/error.c"
  "${vkd3d_shader_source_SOURCE_DIR}/libs/vkd3d-common/memory.c"
  "${vkd3d_shader_source_SOURCE_DIR}/libs/vkd3d-common/utf8.c"
  "${VKD3D_SHADER_SRC}/checksum.c"
  "${VKD3D_SHADER_SRC}/d3d_asm.c"
  "${VKD3D_SHADER_SRC}/d3dbc.c"
  "${VKD3D_SHADER_SRC}/dxbc.c"
  "${VKD3D_SHADER_SRC}/dxil.c"
  "${VKD3D_SHADER_SRC}/fx.c"
  "${VKD3D_SHADER_SRC}/glsl.c"
  "${VKD3D_SHADER_SRC}/hlsl.c"
  "${VKD3D_SHADER_SRC}/hlsl_codegen.c"
  "${VKD3D_SHADER_SRC}/hlsl_constant_ops.c"
  "${VKD3D_SHADER_SRC}/ir.c"
  "${VKD3D_SHADER_SRC}/msl.c"
  "${VKD3D_SHADER_SRC}/spirv.c"
  "${VKD3D_SHADER_SRC}/tpf.c"
  "${VKD3D_SHADER_SRC}/vkd3d_shader_main.c"
  ${FLEX_vkd3d_preproc_OUTPUTS} ${BISON_vkd3d_preproc_OUTPUTS}
  ${FLEX_vkd3d_hlsl_OUTPUTS} ${BISON_vkd3d_hlsl_OUTPUTS})
target_include_directories(vkd3d_shader PUBLIC "${VKD3D_SHADER_INC}"
  PRIVATE "${VKD3D_SHADER_GENERATED}" "${VKD3D_SHADER_GENERATED}/private"
  "${VKD3D_SHADER_PRIVATE}" "${VKD3D_SHADER_SRC}" "${spirv_headers_SOURCE_DIR}/include"
  "${CMAKE_SOURCE_DIR}/third_party/vulkan-headers/include")
target_compile_definitions(vkd3d_shader PUBLIC LIBVKD3D_SHADER_SOURCE
  PRIVATE
  PACKAGE_VERSION=\"2.1\" VKD3D_DEBUG_ENV=VKD3D_SHADER_DEBUG
  HAVE_SYNC_ADD_AND_FETCH HAVE_SYNC_BOOL_COMPARE_AND_SWAP HAVE_ATOMIC_EXCHANGE_N)
if(WIN32)
  if(NOT MINGW)
    file(CONFIGURE OUTPUT "${VKD3D_SHADER_GENERATED}/unistd.h" CONTENT "#include <io.h>\n")
  endif()
  target_compile_definitions(vkd3d_shader PRIVATE HAVE__STRTOF_L _USE_MATH_DEFINES)
elseif(APPLE)
  target_compile_definitions(vkd3d_shader PRIVATE HAVE_STRTOF_L HAVE_XLOCALE_H HAVE_PTHREAD_H
    _DARWIN_C_SOURCE)
else()
  target_compile_definitions(vkd3d_shader PRIVATE HAVE_PTHREAD_H)
  if(VKD3D_HAVE_STRTOF_L)
    target_compile_definitions(vkd3d_shader PRIVATE HAVE_STRTOF_L _GNU_SOURCE)
  endif()
  if(VKD3D_HAVE_XLOCALE_H)
    target_compile_definitions(vkd3d_shader PRIVATE HAVE_XLOCALE_H)
  endif()
  target_link_libraries(vkd3d_shader PRIVATE m)
endif()
target_link_libraries(vkd3d_shader PRIVATE Threads::Threads)
if(CMAKE_C_COMPILER_ID MATCHES "Clang|GNU")
  target_compile_options(vkd3d_shader PRIVATE -Wno-ignored-attributes -Wno-unknown-attributes)
endif()
