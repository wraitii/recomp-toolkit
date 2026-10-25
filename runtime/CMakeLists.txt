# The guest runtime. Object libraries, so every source is linked into every
# consumer exactly as the shell scripts linked every object.
set(RECOMP_RUNTIME_SOURCES
  memory.cpp guest_strings.cpp loader.cpp imports.cpp cpu.cpp dispatch.cpp thunks.cpp seh.cpp profile.cpp reloc.cpp kernel32.cpp kernel32_wide.cpp
  msvcr90.cpp steam_api.cpp
  resources.cpp oleaut32.cpp mods_seam.cpp user32.cpp user32_wide.cpp user32_vcl.cpp user32_static.cpp user32_cursor.cpp user32_dialog.cpp user32_messagebox.cpp
  misc.cpp misc_dlls.cpp comctl32.cpp gdi32.cpp gdi32_draw.cpp gdi32_text.cpp gdi32_truetype.cpp msimg32.cpp
  media_foundation.cpp layout.cpp key_names.cpp interp.cpp discovery.cpp imagehlp.cpp call_trace.cpp linear_trace.cpp
  observe.cpp installation_manifest.cpp ../host/controls/json.cpp)

# The faces the runtime's GDI draws Windows' own UI fonts with
# (third_party/fonts/opensans/README.md), embedded as byte arrays.
set(RECOMP_BUNDLED_FONTS_DIR ${RECOMP_ROOT}/third_party/fonts/opensans)
set(RECOMP_BUNDLED_FONTS_CPP ${CMAKE_CURRENT_BINARY_DIR}/generated/bundled_fonts.cpp)
add_custom_command(
  OUTPUT ${RECOMP_BUNDLED_FONTS_CPP}
  COMMAND ${CMAKE_COMMAND} -DOUT=${RECOMP_BUNDLED_FONTS_CPP} -DNAMESPACE=gdi
          "-DENTRIES=bundled_sans_regular=${RECOMP_BUNDLED_FONTS_DIR}/OpenSans-Regular.ttf|bundled_sans_semibold=${RECOMP_BUNDLED_FONTS_DIR}/OpenSans-Semibold.ttf"
          -P ${RECOMP_ROOT}/cmake/EmbedFiles.cmake
  DEPENDS ${RECOMP_ROOT}/cmake/EmbedFiles.cmake
          ${RECOMP_BUNDLED_FONTS_DIR}/OpenSans-Regular.ttf
          ${RECOMP_BUNDLED_FONTS_DIR}/OpenSans-Semibold.ttf
  COMMENT "Embedding the bundled fonts"
  VERBATIM)
list(APPEND RECOMP_RUNTIME_SOURCES ${RECOMP_BUNDLED_FONTS_CPP})

set(RECOMP_PCODE_LIB "" CACHE FILEPATH "Rust p-code shared library; empty disables p-code fallback")
if(RECOMP_PCODE_LIB)
  add_library(recomp_pcode SHARED IMPORTED GLOBAL)
  set_target_properties(recomp_pcode PROPERTIES IMPORTED_LOCATION "${RECOMP_PCODE_LIB}")
  if(UNIX AND NOT APPLE)
    set_target_properties(recomp_pcode PROPERTIES IMPORTED_NO_SONAME TRUE)
  endif()
  list(APPEND RECOMP_RUNTIME_SOURCES pcode.cpp pcode_x86.cpp)
endif()
add_library(recomp_runtime OBJECT ${RECOMP_RUNTIME_SOURCES})
target_include_directories(recomp_runtime PUBLIC ${RECOMP_ROOT} ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(recomp_runtime PUBLIC recomp_platform)
if(RECOMP_PCODE_LIB)
  target_compile_definitions(recomp_runtime PRIVATE RECOMP_PCODE_FALLBACK=1)
  target_link_libraries(recomp_runtime PUBLIC recomp_pcode)
else()
  target_compile_definitions(recomp_runtime PRIVATE RECOMP_PCODE_FALLBACK=0)
endif()
target_compile_options(recomp_runtime PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(recomp_runtime 2)

# POPM_TESTING compiles the test-only entry points (loader reset, owner
# counter seam, scheduler latch reset). None exists in a real build.
if(NOT IOS AND NOT ANDROID)
add_library(recomp_runtime_testing OBJECT ${RECOMP_RUNTIME_SOURCES})
target_include_directories(recomp_runtime_testing PUBLIC ${RECOMP_ROOT} ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(recomp_runtime_testing PUBLIC recomp_platform)
if(RECOMP_PCODE_LIB)
  target_compile_definitions(recomp_runtime_testing PUBLIC RECOMP_PCODE_FALLBACK=1)
  target_link_libraries(recomp_runtime_testing PUBLIC recomp_pcode)
else()
  target_compile_definitions(recomp_runtime_testing PUBLIC RECOMP_PCODE_FALLBACK=0)
endif()
target_compile_definitions(recomp_runtime_testing PUBLIC POPM_TESTING=1)
target_compile_options(recomp_runtime_testing PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(recomp_runtime_testing 1)
endif()

# The interpreter for heap code, on its own: it needs guest memory and
# recomp_call, which the test provides.
if(NOT IOS AND NOT ANDROID)
set(ENTRY_FIXTURE_DIR ${CMAKE_CURRENT_BINARY_DIR}/generated/entry-fixture)
set(ENTRY_FIXTURE_SOURCES
  ${ENTRY_FIXTURE_DIR}/fixture_body.c
  ${ENTRY_FIXTURE_DIR}/chunk_entries_0d000000.c
  ${ENTRY_FIXTURE_DIR}/fixture_table.c)
add_custom_command(
  OUTPUT ${ENTRY_FIXTURE_SOURCES} ${ENTRY_FIXTURE_DIR}/body.h ${ENTRY_FIXTURE_DIR}/fixture_overrides.h
  COMMAND ${Python3_EXECUTABLE} ${RECOMP_ROOT}/tools/recomp/tests/gen_entry_fixture.py ${ENTRY_FIXTURE_DIR}
  DEPENDS ${RECOMP_ROOT}/tools/recomp/tests/gen_entry_fixture.py ${RECOMP_ROOT}/tools/recomp/output.py
  COMMENT "Generating stable-entry dispatch fixture"
  VERBATIM)
foreach(entry_test entry_dispatch_tests entry_dispatch_nohooks_tests)
  add_executable(${entry_test} tests/entry_dispatch_tests.c ${ENTRY_FIXTURE_SOURCES})
  target_include_directories(${entry_test} PRIVATE ${CMAKE_CURRENT_SOURCE_DIR} ${ENTRY_FIXTURE_DIR})
  target_compile_definitions(${entry_test} PRIVATE RECOMP_OVERRIDE_HEADER="fixture_overrides.h")
  target_compile_options(${entry_test} PRIVATE ${RECOMP_WARN_HOST})
  recomp_optimize(${entry_test} 2)
  recomp_test_binary(${entry_test})
  add_test(NAME ${entry_test} COMMAND ${entry_test})
  set_tests_properties(${entry_test} PROPERTIES LABELS nogame)
endforeach()
target_compile_definitions(entry_dispatch_nohooks_tests PRIVATE RECOMP_NO_HOOKS=1)
add_custom_target(dispatch_tests DEPENDS entry_dispatch_tests entry_dispatch_nohooks_tests)

# The env-gated guest call tracer, against the game-free fixture. call_trace
# lives in recomp_runtime with weak table fallbacks; the fixture's strong tables
# override them and provide real entry dispatch to hook.
add_executable(call_trace_tests tests/call_trace_tests.cpp ${ENTRY_FIXTURE_SOURCES} tests/stub_recomp_call.cpp)
target_include_directories(call_trace_tests PRIVATE ${CMAKE_CURRENT_SOURCE_DIR} ${ENTRY_FIXTURE_DIR})
target_compile_definitions(call_trace_tests PRIVATE RECOMP_OVERRIDE_HEADER="fixture_overrides.h")
target_link_libraries(call_trace_tests PRIVATE recomp_platform recomp_runtime recomp_dx_null)
target_compile_options(call_trace_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(call_trace_tests 1)
recomp_test_binary(call_trace_tests)
add_test(NAME call_trace_tests COMMAND call_trace_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(call_trace_tests PROPERTIES LABELS game TIMEOUT 120)

set(INTERP_TEST_SOURCES tests/interp_tests.cpp interp.cpp)
if(RECOMP_PCODE_LIB)
  list(APPEND INTERP_TEST_SOURCES pcode.cpp pcode_x86.cpp)
endif()
add_executable(interp_tests ${INTERP_TEST_SOURCES})
target_include_directories(interp_tests PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(interp_tests PRIVATE recomp_platform)
if(RECOMP_PCODE_LIB)
  target_link_libraries(interp_tests PRIVATE recomp_pcode)
  target_compile_definitions(interp_tests PRIVATE RECOMP_PCODE_FALLBACK=1)
else()
  target_compile_definitions(interp_tests PRIVATE RECOMP_PCODE_FALLBACK=0)
endif()
target_compile_options(interp_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_test_binary(interp_tests)
add_test(NAME interp_tests COMMAND interp_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(interp_tests PROPERTIES LABELS nogame TIMEOUT 120)
endif()

# The suite spawns itself for the abort check; portable since sub-project 3.
if(NOT IOS AND NOT ANDROID)
add_executable(seh_tests tests/seh_tests.cpp)
target_link_libraries(seh_tests PRIVATE recomp_platform recomp_runtime_testing recomp_dx_null)
target_compile_options(seh_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(seh_tests 1)
recomp_test_binary(seh_tests)
add_test(NAME seh_tests COMMAND seh_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(seh_tests PROPERTIES LABELS nogame ENVIRONMENT RECOMP_PROFILE=1)

add_executable(runtime_tests tests/runtime_tests.cpp tests/stub_recomp_call.cpp)
target_link_libraries(runtime_tests PRIVATE recomp_platform recomp_runtime recomp_dx)
target_compile_options(runtime_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(runtime_tests 1)
recomp_test_binary(runtime_tests)
add_test(NAME runtime_tests COMMAND runtime_tests WORKING_DIRECTORY ${RECOMP_ROOT})
# TIMEOUT for the reason interp_tests has one: this suite also runs guest
# code through the interpreter. It takes two seconds when that returns.
set_tests_properties(runtime_tests PROPERTIES LABELS game TIMEOUT 600
                     ENVIRONMENT RECOMP_PYTHON=${Python3_EXECUTABLE})
add_executable(gdi_tests tests/gdi_tests.cpp tests/stub_recomp_call.cpp ${RECOMP_ROOT}/dx/com.cpp)
target_link_libraries(gdi_tests PRIVATE recomp_platform recomp_runtime)
target_compile_options(gdi_tests PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(gdi_tests 1)
recomp_test_binary(gdi_tests)
add_test(NAME gdi_tests COMMAND gdi_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(gdi_tests PROPERTIES LABELS nogame)
add_test(NAME gdi_model_tests COMMAND gdi_tests model WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(gdi_model_tests PROPERTIES LABELS nogame)
add_test(NAME gdi_draw_tests COMMAND gdi_tests draw WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(gdi_draw_tests PROPERTIES LABELS nogame)

endif()

if(RECOMP_REAL_GEN AND NOT IOS AND NOT ANDROID)
  # CALL_FN takes an address token (without 0x), not a numeric expression.
  # Select the first generated entry; define the suite only when one exists.
  set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS ${RECOMP_GEN_DIR}/funcs.h)
  file(STRINGS ${RECOMP_GEN_DIR}/funcs.h PROFILE_FIRST_INDEX
    REGEX "^#define FIDX_[0-9a-f]+ 0u$" LIMIT_COUNT 1)
  if(PROFILE_FIRST_INDEX)
    add_executable(profile_tests tests/profile_tests.cpp)
    add_dependencies(dispatch_tests profile_tests)
    string(REGEX REPLACE "^#define FIDX_([0-9a-f]+) 0u$" "\\1"
      PROFILE_TARGET_TOKEN "${PROFILE_FIRST_INDEX}")
    target_compile_definitions(profile_tests PRIVATE RECOMP_PROFILE_TARGET=${PROFILE_TARGET_TOKEN})
    target_link_libraries(profile_tests PRIVATE recomp_platform recomp_runtime recomp_dx_null)
    target_compile_options(profile_tests PRIVATE ${RECOMP_WARN_HOST})
    recomp_optimize(profile_tests 1)
    recomp_link_gen(profile_tests)
    recomp_test_binary(profile_tests)
    add_test(NAME profile_tests_disabled COMMAND profile_tests disabled WORKING_DIRECTORY ${RECOMP_ROOT})
    set_tests_properties(profile_tests_disabled PROPERTIES LABELS game ENVIRONMENT RECOMP_PROFILE=0)
    add_test(NAME profile_tests_enabled COMMAND profile_tests enabled WORKING_DIRECTORY ${RECOMP_ROOT})
    set_tests_properties(profile_tests_enabled PROPERTIES LABELS game ENVIRONMENT RECOMP_PROFILE=1)
  else()
    message(STATUS "profile_tests: the translation has no FIDX entry; suite not defined")
  endif()
endif()
