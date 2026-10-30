set(RECOMP_DX_SOURCES
  com.cpp dx.cpp host_api.cpp display_stubs.cpp ddraw.cpp d3d.cpp d3d7.cpp d3d11.cpp dxgi.cpp d3dcompiler.cpp
  d3dx10.cpp dsound.cpp dshow.cpp mp3_source.cpp dinput.cpp dinput_joystick.cpp xinput.cpp qmixer.cpp
  mss32.cpp redbook.cpp avi.cpp waveout.cpp fmod.cpp soundlib.cpp galaxy_stub.cpp riff.cpp wav_source.cpp bink.cpp video_frame.cpp mf_media.cpp
  mf.cpp weanetr.cpp d3d9.cpp d3dx9_compile.cpp
  d3dx9.cpp d3d8.cpp)

include(${CMAKE_SOURCE_DIR}/cmake/Vkd3dShader.cmake)

# A static archive lets ordinary hosts share miniz with the launcher without
# duplicating its object, and includes it for hosts that have no launcher.
add_library(recomp_frame_dump STATIC frame_dump.cpp $<TARGET_OBJECTS:recomp_miniz>)
target_link_libraries(recomp_frame_dump PUBLIC recomp_platform)
target_compile_options(recomp_frame_dump PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(recomp_frame_dump 1)

# Interface code is disposable build output; the pinned API header and handler
# choices are source inputs. Both renderer and null-host objects consume it.
set(RECOMP_D3D8_GENERATED_DIR "${RECOMP_BUILD_ROOT}/d3d8-generated")
set(RECOMP_D3D8_COM_TABLES "${RECOMP_D3D8_GENERATED_DIR}/d3d8_interfaces.inc")
add_custom_command(OUTPUT "${RECOMP_D3D8_COM_TABLES}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D8_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8.h"
          --bindings "${CMAKE_CURRENT_SOURCE_DIR}/d3d8_bindings.json"
          --output "${RECOMP_D3D8_COM_TABLES}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8.h" "${CMAKE_CURRENT_SOURCE_DIR}/d3d8_bindings.json"
  VERBATIM)
set(RECOMP_D3D8_CONSTANTS "${RECOMP_D3D8_GENERATED_DIR}/d3d8_constants.inc")
add_custom_command(OUTPUT "${RECOMP_D3D8_CONSTANTS}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D8_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_d3d8_constants.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8types.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8caps.h"
          --names "${CMAKE_CURRENT_SOURCE_DIR}/d3d8_constants.json"
          --output "${RECOMP_D3D8_CONSTANTS}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_d3d8_constants.py"
          "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8types.h"
          "${RECOMP_ROOT}/third_party/wine-d3d8/d3d8caps.h"
          "${CMAKE_CURRENT_SOURCE_DIR}/d3d8_constants.json"
  VERBATIM)
add_custom_target(d3d8_com_tables DEPENDS "${RECOMP_D3D8_COM_TABLES}" "${RECOMP_D3D8_CONSTANTS}")

set(RECOMP_D3D9_GENERATED_DIR "${RECOMP_BUILD_ROOT}/d3d9-generated")
set(RECOMP_D3D9_INTERFACES "${RECOMP_D3D9_GENERATED_DIR}/d3d9_interfaces.inc")
add_custom_command(OUTPUT "${RECOMP_D3D9_INTERFACES}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D9_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d9/d3d9.h"
          --bindings "${CMAKE_CURRENT_SOURCE_DIR}/d3d9_bindings.json"
          --sdk-version 33
          --output "${RECOMP_D3D9_INTERFACES}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3d9.h" "${CMAKE_CURRENT_SOURCE_DIR}/d3d9_bindings.json"
  VERBATIM)
set(RECOMP_D3D9_CONSTANTS "${RECOMP_D3D9_GENERATED_DIR}/d3d9_constants.inc")
add_custom_command(OUTPUT "${RECOMP_D3D9_CONSTANTS}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D9_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_d3d8_constants.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d9/d3d9types.h"
          --names "${CMAKE_CURRENT_SOURCE_DIR}/d3d9_constants.json"
          --output "${RECOMP_D3D9_CONSTANTS}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_d3d8_constants.py"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3d9types.h"
          "${CMAKE_CURRENT_SOURCE_DIR}/d3d9_constants.json"
  VERBATIM)
set(RECOMP_D3DX9_33_INTERFACES "${RECOMP_D3D9_GENERATED_DIR}/d3dx9_33_interfaces.inc")
add_custom_command(OUTPUT "${RECOMP_D3DX9_33_INTERFACES}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D9_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9core.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9shader.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9effect.h"
          --bindings "${CMAKE_CURRENT_SOURCE_DIR}/d3dx9_33_bindings.json"
          --sdk-version 33
          --suffix _33
          --output "${RECOMP_D3DX9_33_INTERFACES}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9core.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9shader.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9effect.h" "${CMAKE_CURRENT_SOURCE_DIR}/d3dx9_33_bindings.json"
  VERBATIM)
set(RECOMP_D3DX9_IMPORTS "${RECOMP_D3D9_GENERATED_DIR}/d3dx9_imports.inc")
add_custom_command(OUTPUT "${RECOMP_D3DX9_IMPORTS}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D9_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_d3dx9_imports.py"
          --reference "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9core.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9effect.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9math.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9shader.h"
                      "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9tex.h"
          --imports "${CMAKE_CURRENT_SOURCE_DIR}/d3dx9_imports.json"
          --sdk-version 33
          --output "${RECOMP_D3DX9_IMPORTS}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_d3dx9_imports.py" "${RECOMP_ROOT}/tools/gen_com_interfaces.py"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9core.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9effect.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9math.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9shader.h"
          "${RECOMP_ROOT}/third_party/wine-d3d9/d3dx9tex.h"
          "${CMAKE_CURRENT_SOURCE_DIR}/d3dx9_imports.json"
  VERBATIM)
set(RECOMP_COM_HOST_SLOTS "${RECOMP_D3D9_GENERATED_DIR}/com_host_slots.inc")
add_custom_command(OUTPUT "${RECOMP_COM_HOST_SLOTS}"
  COMMAND ${CMAKE_COMMAND} -E make_directory "${RECOMP_D3D9_GENERATED_DIR}"
  COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/gen_com_host_slots.py"
          --catalog "${CMAKE_CURRENT_SOURCE_DIR}/metadata/com-methods.json"
          --output "${RECOMP_COM_HOST_SLOTS}"
  DEPENDS "${RECOMP_ROOT}/tools/gen_com_host_slots.py"
          "${CMAKE_CURRENT_SOURCE_DIR}/metadata/com-methods.json"
  VERBATIM)
add_custom_target(d3d9_generated DEPENDS "${RECOMP_D3D9_INTERFACES}" "${RECOMP_D3D9_CONSTANTS}"
                  "${RECOMP_D3DX9_33_INTERFACES}" "${RECOMP_D3DX9_IMPORTS}"
                  "${RECOMP_COM_HOST_SLOTS}")

add_library(recomp_dx OBJECT ${RECOMP_DX_SOURCES})
add_dependencies(recomp_dx d3d8_com_tables d3d9_generated)
target_include_directories(recomp_dx PUBLIC "${RECOMP_D3D8_GENERATED_DIR}" "${RECOMP_D3D9_GENERATED_DIR}")
target_link_libraries(recomp_dx PUBLIC recomp_runtime recomp_frame_dump vkd3d_shader)
recomp_link_video(recomp_dx)
target_compile_options(recomp_dx PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(recomp_dx 2)

option(RECOMP_D3D8_WGPU "Enable Rust GPU rendering in native shims" OFF)
set(RECOMP_RENDERER_CARGO_TARGET "" CACHE STRING "Native Cargo target triple for the renderer")
if(EXISTS "${RECOMP_GAME_DIR}/graphics/d3d8-wgpu/Cargo.toml")
  message(FATAL_ERROR "Per-game renderer overrides are unsupported; use the kit graphics workspace so native headers and Rust libraries agree")
endif()
set(RECOMP_GRAPHICS_DIR "${RECOMP_ROOT}/graphics")
set(RECOMP_GRAPHICS_DIR "${RECOMP_GRAPHICS_DIR}" PARENT_SCOPE)
set(RECOMP_D3D8_WGPU_DIR "${RECOMP_GRAPHICS_DIR}/d3d8-wgpu")
if(RECOMP_DEPENDENCIES)
  if(RECOMP_RENDERER_CARGO_TARGET)
    message(FATAL_ERROR "An explicit renderer Cargo target cannot be used with staged dependencies")
  endif()
  target_include_directories(recomp_dx BEFORE PUBLIC "${RECOMP_DEPENDENCIES}/d3d8")
  add_custom_target(d3d_bridge_headers DEPENDS
    "${RECOMP_DEPENDENCIES}/d3d8/d3d_bridge.h" "${RECOMP_DEPENDENCIES}/d3d8/rust/cxx.h")
elseif(EXISTS "${RECOMP_D3D8_WGPU_DIR}/Cargo.toml")
  include(${RECOMP_ROOT}/cmake/RustGraphics.cmake)
  file(GLOB_RECURSE RECOMP_D3D8_WGPU_SOURCES CONFIGURE_DEPENDS
    "${RECOMP_D3D8_WGPU_DIR}/src/*.rs"
    "${RECOMP_D3D8_WGPU_DIR}/src/*.wgsl"
    "${RECOMP_GRAPHICS_DIR}/gpu/src/*.rs"
    "${RECOMP_GRAPHICS_DIR}/compositor/src/*.rs"
    "${RECOMP_GRAPHICS_DIR}/compositor/src/*.wgsl")
  set(RECOMP_D3D8_WGPU_TARGET_DIR "${RECOMP_BUILD_ROOT}/d3d8-wgpu/rust")
  set(RECOMP_D3D8_OUTPUT_DIR "${RECOMP_D3D8_WGPU_TARGET_DIR}${RECOMP_GRAPHICS_TARGET_SUFFIX}")
  set(RECOMP_D3D8_WGPU_LIB "${RECOMP_D3D8_OUTPUT_DIR}/release/${CMAKE_STATIC_LIBRARY_PREFIX}d3d8_wgpu${CMAKE_STATIC_LIBRARY_SUFFIX}")
  set(RECOMP_D3D8_BRIDGE_DIR "${RECOMP_D3D8_GENERATED_DIR}")
  if(RECOMP_RENDERER_CARGO_TARGET)
    string(APPEND RECOMP_D3D8_BRIDGE_DIR "/${RECOMP_RENDERER_CARGO_TARGET}")
    target_include_directories(recomp_dx BEFORE PUBLIC "${RECOMP_D3D8_BRIDGE_DIR}")
  endif()
  set(RECOMP_D3D8_NATIVE_LIBS "${RECOMP_D3D8_BRIDGE_DIR}/native-libs.rsp")
  set(RECOMP_D3D8_BRIDGE_HEADER "${RECOMP_D3D8_BRIDGE_DIR}/d3d_bridge.h")
  set(RECOMP_D3D8_CXX_HEADER "${RECOMP_D3D8_BRIDGE_DIR}/rust/cxx.h")
  if(WIN32 AND CMAKE_CXX_COMPILER_ID STREQUAL "Clang" AND CMAKE_CXX_SIMULATE_ID STREQUAL "MSVC")
    get_filename_component(RECOMP_D3D8_COMPILER_DIR "${CMAKE_CXX_COMPILER}" DIRECTORY)
    find_program(RECOMP_D3D8_CXX NAMES clang-cl HINTS "${RECOMP_D3D8_COMPILER_DIR}" REQUIRED)
  else()
    set(RECOMP_D3D8_CXX "${CMAKE_CXX_COMPILER}")
  endif()
  add_custom_command(
    OUTPUT "${RECOMP_D3D8_WGPU_LIB}"
           "${RECOMP_D3D8_BRIDGE_HEADER}" "${RECOMP_D3D8_CXX_HEADER}" "${RECOMP_D3D8_NATIVE_LIBS}"
    COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/build_d3d8_wgpu.py"
            --cargo "${RECOMP_GRAPHICS_CARGO}" --crate "${RECOMP_D3D8_WGPU_DIR}"
            --target-dir "${RECOMP_D3D8_WGPU_TARGET_DIR}"
            --bridge-header "${RECOMP_D3D8_BRIDGE_HEADER}" --cxx-header "${RECOMP_D3D8_CXX_HEADER}"
            --native-libs "${RECOMP_D3D8_NATIVE_LIBS}" --cxx "${RECOMP_D3D8_CXX}"
            ${RECOMP_GRAPHICS_DEPLOYMENT_ARGS} ${RECOMP_GRAPHICS_TARGET_ARGS}
    DEPENDS "${RECOMP_D3D8_WGPU_DIR}/Cargo.toml"
            "${RECOMP_GRAPHICS_DIR}/Cargo.lock"
            "${RECOMP_GRAPHICS_DIR}/Cargo.toml"
            "${RECOMP_D3D8_WGPU_DIR}/build.rs"
            "${RECOMP_GRAPHICS_DIR}/gpu/Cargo.toml"
            "${RECOMP_GRAPHICS_DIR}/compositor/Cargo.toml"
            "${RECOMP_ROOT}/tools/build_d3d8_wgpu.py"
            "${RECOMP_ROOT}/tools/cargo_build.py"
            ${RECOMP_D3D8_WGPU_SOURCES}
    COMMENT "Building d3d8-wgpu Rust static library"
    VERBATIM)
  add_custom_target(d3d_bridge_headers DEPENDS
    "${RECOMP_D3D8_BRIDGE_HEADER}" "${RECOMP_D3D8_CXX_HEADER}")
  add_dependencies(recomp_dx d3d_bridge_headers)
  add_custom_target(d3d8_wgpu_rust DEPENDS "${RECOMP_D3D8_WGPU_LIB}")
else()
  message(FATAL_ERROR "D3D7/D3D8 Rust host crate was not found")
endif()
if(RECOMP_DEPENDENCIES)
  set(RECOMP_D3D8_WGPU_LIB "${RECOMP_DEPENDENCIES}/d3d8/libd3d8_wgpu.a")
  set(RECOMP_D3D8_NATIVE_LIBS "${RECOMP_DEPENDENCIES}/d3d8/native-libs.rsp")
endif()
add_library(d3d8_wgpu STATIC IMPORTED GLOBAL)
if(RECOMP_DEPENDENCIES)
  set_target_properties(d3d8_wgpu PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${RECOMP_DEPENDENCIES}/d3d8")
endif()
set_target_properties(d3d8_wgpu PROPERTIES IMPORTED_LOCATION "${RECOMP_D3D8_WGPU_LIB}")
if(MSVC)
  set_target_properties(d3d8_wgpu PROPERTIES INTERFACE_LINK_OPTIONS "@${RECOMP_D3D8_NATIVE_LIBS}")
else()
  set_target_properties(d3d8_wgpu PROPERTIES INTERFACE_LINK_OPTIONS "SHELL:-Wl,@\"${RECOMP_D3D8_NATIVE_LIBS}\"")
endif()
if(TARGET d3d8_wgpu_rust)
  add_dependencies(d3d8_wgpu d3d8_wgpu_rust)
endif()
if(RECOMP_D3D8_WGPU)
  target_compile_definitions(recomp_dx PRIVATE RECOMP_D3D8_WGPU=1)
endif()
target_link_libraries(recomp_dx PUBLIC d3d8_wgpu)

# The same shims with every host callback a strong no-op: a binary linking
# these can neither draw nor play sound. Profile and mod tests use it.
add_library(recomp_dx_null OBJECT ${RECOMP_DX_SOURCES})
add_dependencies(recomp_dx_null d3d8_com_tables d3d9_generated)
target_include_directories(recomp_dx_null PUBLIC "${RECOMP_D3D8_GENERATED_DIR}" "${RECOMP_D3D9_GENERATED_DIR}")
if(RECOMP_RENDERER_CARGO_TARGET AND RECOMP_D3D8_BRIDGE_DIR)
  target_include_directories(recomp_dx_null BEFORE PUBLIC "${RECOMP_D3D8_BRIDGE_DIR}")
endif()
target_link_libraries(recomp_dx_null PUBLIC recomp_runtime recomp_frame_dump)
recomp_link_video(recomp_dx_null)
target_compile_definitions(recomp_dx_null PUBLIC RECOMP_NULL_HOST)
target_compile_options(recomp_dx_null PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(recomp_dx_null 1)
if(TARGET d3d8_wgpu)
  if(TARGET d3d_bridge_headers)
    add_dependencies(recomp_dx_null d3d_bridge_headers)
  else()
    target_include_directories(recomp_dx_null BEFORE PUBLIC "${RECOMP_DEPENDENCIES}/d3d8")
  endif()
  target_link_libraries(recomp_dx_null PUBLIC d3d8_wgpu)
endif()


if(NOT IOS AND NOT ANDROID)
# Descriptor/lifetime tests use CPU resources and GPU-backed render-target fixtures.
add_executable(d3d8_surface_tests tests/d3d8_surface_tests.cpp
  ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
target_link_libraries(d3d8_surface_tests PRIVATE recomp_platform recomp_dx_null recomp_runtime)
target_compile_options(d3d8_surface_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(d3d8_surface_tests 1)
recomp_test_binary(d3d8_surface_tests)
add_test(NAME d3d8_surface_tests COMMAND d3d8_surface_tests)
set_tests_properties(d3d8_surface_tests PROPERTIES LABELS gpu)
if(TARGET d3d8_wgpu)
  add_executable(d3d9_guest_tests tests/d3d9_guest_tests.cpp
    ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
  add_dependencies(d3d9_guest_tests d3d_bridge_headers)
  target_link_libraries(d3d9_guest_tests PRIVATE recomp_platform recomp_dx recomp_runtime)
  target_compile_options(d3d9_guest_tests PRIVATE ${RECOMP_WARN_HOST})
  recomp_optimize(d3d9_guest_tests 1)
  recomp_test_binary(d3d9_guest_tests)
  add_test(NAME d3d9_guest_tests COMMAND d3d9_guest_tests)
  set_tests_properties(d3d9_guest_tests PROPERTIES LABELS gpu)
  add_executable(d3d8_render_tests tests/d3d8_render_tests.cpp
    ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
  add_dependencies(d3d8_render_tests d3d_bridge_headers)
  target_include_directories(d3d8_render_tests PRIVATE "${RECOMP_D3D8_GENERATED_DIR}")
  target_link_libraries(d3d8_render_tests PRIVATE recomp_platform recomp_dx recomp_runtime)
  target_compile_options(d3d8_render_tests PRIVATE ${RECOMP_WARN_HOST})
  recomp_optimize(d3d8_render_tests 1)
  recomp_test_binary(d3d8_render_tests)
  add_test(NAME d3d8_render_tests COMMAND d3d8_render_tests)
  set_tests_properties(d3d8_render_tests PROPERTIES LABELS gpu)
  add_executable(d3d7_mipmap_tests tests/d3d7_mipmap_tests.cpp
    ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
  add_dependencies(d3d7_mipmap_tests d3d_bridge_headers)
  target_include_directories(d3d7_mipmap_tests PRIVATE "${RECOMP_D3D8_GENERATED_DIR}")
  target_link_libraries(d3d7_mipmap_tests PRIVATE recomp_platform recomp_dx recomp_runtime)
  target_compile_options(d3d7_mipmap_tests PRIVATE ${RECOMP_WARN_HOST})
  recomp_optimize(d3d7_mipmap_tests 1)
  recomp_test_binary(d3d7_mipmap_tests)
  add_test(NAME d3d7_mipmap_tests COMMAND d3d7_mipmap_tests)
  set_tests_properties(d3d7_mipmap_tests PROPERTIES LABELS gpu)
endif()


# host_api.h is a C header too. The C11 consumer is an object the DX tests
# link; the header is also parsed as C++17 on its own.
add_library(host_api_header_test OBJECT tests/host_api_header_test.c)
target_include_directories(host_api_header_test PRIVATE ${RECOMP_ROOT})
target_compile_options(host_api_header_test PRIVATE -Wall -Wextra)
recomp_optimize(host_api_header_test 1)
add_test(NAME host_api_header_check
  COMMAND ${CMAKE_CXX_COMPILER} -std=c++17 -O1 -Wall -Wextra -Wno-unused-parameter
          -I${RECOMP_ROOT} -fsyntax-only -x c++ ${CMAKE_CURRENT_SOURCE_DIR}/host_api.h)
set_tests_properties(host_api_header_check PROPERTIES LABELS nogame)

add_executable(dx_tests tests/dx_tests.cpp
  ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp
  $<TARGET_OBJECTS:host_api_header_test>)
target_link_libraries(dx_tests PRIVATE recomp_platform recomp_dx recomp_runtime)
target_compile_options(dx_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(dx_tests 1)
recomp_test_binary(dx_tests)
add_test(NAME dx_tests COMMAND dx_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(dx_tests PROPERTIES LABELS nogame)

# The virtual pad through the DirectInput joystick and XInput. Its own binary
# because it defines the host_pad_* callbacks strongly, which dx_tests must not.
add_executable(pad_tests tests/pad_tests.cpp
  ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
target_link_libraries(pad_tests PRIVATE recomp_platform recomp_dx recomp_runtime)
target_compile_options(pad_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(pad_tests 1)
recomp_test_binary(pad_tests)
add_test(NAME pad_tests COMMAND pad_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(pad_tests PROPERTIES LABELS nogame)

# No strong host callback anywhere: the weak defaults must carry the link.
# page_overlay.cpp because every host links it beside boot.cpp; the dispatch
# stub because no generated code is present.
add_executable(null_host_link tests/null_host_link.cpp
  ${RECOMP_ROOT}/host/page_overlay.cpp
  ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp)
target_include_directories(null_host_link PRIVATE ${RECOMP_ROOT})
target_link_libraries(null_host_link PRIVATE recomp_platform recomp_runtime recomp_dx host_common)
target_compile_options(null_host_link PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(null_host_link 1)
recomp_test_binary(null_host_link)
add_test(NAME null_host_link COMMAND null_host_link WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(null_host_link PROPERTIES LABELS nogame)
endif()
