if(RECOMP_DEPENDENCIES)
  set(RECOMP_COMPOSITOR_LIB "${RECOMP_DEPENDENCIES}/compositor/librecomp_compositor.a")
  set(RECOMP_COMPOSITOR_NATIVE_LIBS "${RECOMP_DEPENDENCIES}/compositor/native-libs.rsp")
  if(NOT EXISTS "${RECOMP_COMPOSITOR_LIB}" OR NOT EXISTS "${RECOMP_COMPOSITOR_NATIVE_LIBS}")
    message(FATAL_ERROR "Staged compositor library or native link dependencies are missing from ${RECOMP_DEPENDENCIES}/compositor")
  endif()
else()
  include(${RECOMP_ROOT}/cmake/RustGraphics.cmake)
  set(RECOMP_COMPOSITOR_DIR "${RECOMP_GRAPHICS_DIR}/compositor")
  set(RECOMP_COMPOSITOR_TARGET_DIR "${RECOMP_BUILD_ROOT}/compositor/rust")
  set(RECOMP_COMPOSITOR_OUTPUT_DIR "${RECOMP_COMPOSITOR_TARGET_DIR}${RECOMP_GRAPHICS_TARGET_SUFFIX}")
  set(RECOMP_COMPOSITOR_LIB "${RECOMP_COMPOSITOR_OUTPUT_DIR}/release/${CMAKE_STATIC_LIBRARY_PREFIX}recomp_compositor${CMAKE_STATIC_LIBRARY_SUFFIX}")
  set(RECOMP_COMPOSITOR_NATIVE_LIBS "${RECOMP_COMPOSITOR_OUTPUT_DIR}/native-libs.rsp")
  file(GLOB_RECURSE RECOMP_COMPOSITOR_SOURCES CONFIGURE_DEPENDS "${RECOMP_COMPOSITOR_DIR}/src/*.rs")
  add_custom_command(
    OUTPUT "${RECOMP_COMPOSITOR_LIB}" "${RECOMP_COMPOSITOR_NATIVE_LIBS}"
    COMMAND ${Python3_EXECUTABLE} "${RECOMP_ROOT}/tools/build_compositor.py"
            --cargo "${RECOMP_GRAPHICS_CARGO}" --crate "${RECOMP_COMPOSITOR_DIR}"
            --target-dir "${RECOMP_COMPOSITOR_TARGET_DIR}" --native-libs "${RECOMP_COMPOSITOR_NATIVE_LIBS}"
            ${RECOMP_GRAPHICS_TARGET_ARGS} ${RECOMP_GRAPHICS_DEPLOYMENT_ARGS}
    DEPENDS "${RECOMP_COMPOSITOR_DIR}/Cargo.toml" "${RECOMP_GRAPHICS_DIR}/Cargo.lock" "${RECOMP_GRAPHICS_DIR}/Cargo.toml"
            "${RECOMP_ROOT}/tools/build_compositor.py" "${RECOMP_ROOT}/tools/cargo_build.py"
            ${RECOMP_COMPOSITOR_SOURCES}
    COMMENT "Building Rust compositor" VERBATIM)
  add_custom_target(recomp_compositor_rust DEPENDS "${RECOMP_COMPOSITOR_LIB}" "${RECOMP_COMPOSITOR_NATIVE_LIBS}")
endif()
add_library(recomp_compositor STATIC IMPORTED GLOBAL)
set_target_properties(recomp_compositor PROPERTIES IMPORTED_LOCATION "${RECOMP_COMPOSITOR_LIB}")
if(MSVC)
  set_target_properties(recomp_compositor PROPERTIES
    INTERFACE_LINK_OPTIONS "@${RECOMP_COMPOSITOR_NATIVE_LIBS}")
else()
  set_target_properties(recomp_compositor PROPERTIES
    INTERFACE_LINK_OPTIONS "SHELL:-Wl,@\"${RECOMP_COMPOSITOR_NATIVE_LIBS}\"")
endif()
if(TARGET recomp_compositor_rust)
  add_dependencies(recomp_compositor recomp_compositor_rust)
endif()
