# The translated game lives only in the developer's build/recomp/gen; the
# kit never tracks generated code (spec section 11). Generated C is platform
# independent, so every preset (macOS, iOS) compiles the same directory.
set(RECOMP_GEN_DIR ${RECOMP_BUILD_ROOT}/recomp/gen)
if(RECOMP_TRANSLATE STREQUAL "STUB")
  # A link-only translation for builds without game code (CI).
  set(RECOMP_GEN_DIR ${CMAKE_BINARY_DIR}/stub-gen)
  execute_process(
    COMMAND ${Python3_EXECUTABLE} ${RECOMP_ROOT}/tools/gen_stub_translation.py --out ${RECOMP_GEN_DIR}
    RESULT_VARIABLE RECOMP_STUB_RESULT)
  if(NOT RECOMP_STUB_RESULT EQUAL 0)
    message(FATAL_ERROR "tools/gen_stub_translation.py failed")
  endif()
endif()
set(RECOMP_HAVE_GEN OFF)
# RECOMP_REAL_GEN: the translation is the game's, not the link-only stub. Tests
# that call into generated functions by address are defined only then.
set(RECOMP_REAL_GEN OFF)
if(RECOMP_TRANSLATE STREQUAL "OFF")
  message(STATUS "RECOMP_TRANSLATE=OFF: targets that need the generated code are not defined")
elseif(EXISTS ${RECOMP_GEN_DIR}/table.c)
  set(RECOMP_HAVE_GEN ON)
  if(NOT RECOMP_TRANSLATE STREQUAL "STUB")
    set(RECOMP_REAL_GEN ON)
  endif()
  message(STATUS "Translation: ${RECOMP_GEN_DIR}")
elseif(RECOMP_TRANSLATE STREQUAL "ON")
  message(FATAL_ERROR "RECOMP_TRANSLATE=ON but no translation: run tools/build.py --regenerate")
else()
  message(STATUS "No translation found: hosts and game-backed tests are not defined")
endif()

# New translations isolate overrides in table.c and the entry thunks. Keep old generated trees usable
# until their next regeneration; those still need the legacy target-wide define.
function(recomp_translation_overrides target dir header)
  if(NOT header)
    return()
  endif()
  if(NOT EXISTS "${header}")
    message(FATAL_ERROR "RECOMP_OVERRIDE_HEADER does not exist: ${header}")
  endif()
  get_filename_component(override_dir "${header}" DIRECTORY)
  if(EXISTS "${dir}/body.h")
    file(GLOB selecting CONFIGURE_DEPENDS ${dir}/table.c ${dir}/chunk_entries_*.c)
    set_property(SOURCE ${selecting} APPEND PROPERTY
      COMPILE_DEFINITIONS RECOMP_OVERRIDE_HEADER="${header}")
    set_property(SOURCE ${selecting} APPEND PROPERTY INCLUDE_DIRECTORIES "${override_dir}")
  else()
    target_compile_definitions(${target} PRIVATE RECOMP_OVERRIDE_HEADER="${header}")
    target_include_directories(${target} PRIVATE "${override_dir}")
  endif()
endfunction()

# Generated bodies keep line tables for profiling without full debug-info cost.
function(recomp_generated_source_options)
  set_property(SOURCE ${ARGN} APPEND PROPERTY COMPILE_OPTIONS
    "$<$<C_COMPILER_ID:Clang,AppleClang>:-Xclang;-disable-lifetime-markers>"
    "$<$<AND:$<NOT:$<CONFIG:Debug>>,$<C_COMPILER_ID:Clang,AppleClang>>:-gline-tables-only>"
    "$<$<AND:$<NOT:$<CONFIG:Debug>>,$<C_COMPILER_ID:GNU>>:-g1>")
endfunction()

# Submit isolated giants first so their compile can overlap the small chunks.
function(recomp_translation_sources result dir)
  file(GLOB sources CONFIGURE_DEPENDS ${dir}/chunk_*.c ${dir}/table.c ${dir}/call_facts.c)
  recomp_generated_source_options(${sources})
  set(giants ${sources})
  list(FILTER giants INCLUDE REGEX "/chunk_fn_[0-9a-f]+\\.c$")
  if(giants)
    list(REMOVE_ITEM sources ${giants})
  endif()
  set(${result} ${giants} ${sources} PARENT_SCOPE)
endfunction()

set(RECOMP_EFFECTIVE_OVERRIDE "${RECOMP_OVERRIDE_HEADER}")
if(RECOMP_NATIVE_HEADER AND NOT RECOMP_TRANSLATE STREQUAL "STUB")
  set(RECOMP_EFFECTIVE_OVERRIDE "${RECOMP_NATIVE_HEADER}")
endif()

# The main image's bodies are split into address-ordered dylibs: a single
# translation's text exceeds arm64's +/-128 MiB branch range. Entry thunks
# travel with the bodies of their address, so calls inside a piece stay direct
# and the app keeps only the dispatch table. Other platforms link the whole
# translation as one archive, where the branch range is not a limit.
set(RECOMP_GEN_PIECES "")
set(RECOMP_GEN_PIECE_BUDGET 160000000 CACHE STRING "C source bytes per translated dylib")
option(RECOMP_SPLIT_TRANSLATION "Split desktop macOS translations into dylibs" OFF)

function(recomp_make_piece index)
  set(target recomp_piece_${index})
  add_library(${target} SHARED ${ARGN})
  set_target_properties(${target} PROPERTIES
    POSITION_INDEPENDENT_CODE ON
    INSTALL_NAME_DIR "@rpath" OUTPUT_NAME ${target})
  # The runtime and the main table stay in the app; resolve their symbols at
  # load time instead of linking a second copy.
  target_link_options(${target} PRIVATE "-Wl,-undefined,dynamic_lookup")
  target_include_directories(${target} PRIVATE ${RECOMP_GEN_DIR} ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime)
  target_compile_options(${target} PRIVATE ${RECOMP_WARN_GEN})
  recomp_generated_source_options(${ARGN})
  recomp_optimize(${target} 2)
endfunction()

function(recomp_split_main_gen result)
  file(GLOB chunks CONFIGURE_DEPENDS ${RECOMP_GEN_DIR}/chunk_*.c)
  set(ordered "")
  foreach(src IN LISTS chunks)
    get_filename_component(name "${src}" NAME)
    set(key "")
    if(name MATCHES "^chunk_entries_([0-9a-f]+)\\.c$")
      set(key "${CMAKE_MATCH_1}")
    elseif(name MATCHES "^chunk_fn_([0-9a-f]+)\\.c$")
      set(key "${CMAKE_MATCH_1}")
    elseif(name MATCHES "^chunk_([0-9a-f]+)_[0-9]+\\.c$")
      set(key "${CMAKE_MATCH_1}")
    endif()
    if(key)
      list(APPEND ordered "${key}/${src}")
    endif()
  endforeach()
  list(SORT ordered)
  set(sources "")
  set(bytes 0)
  set(index 0)
  set(targets "")
  foreach(item IN LISTS ordered)
    string(REGEX REPLACE "^[0-9a-f]+/" "" src "${item}")
    file(SIZE "${src}" size)
    math(EXPR total "${bytes} + ${size}")
    if(sources AND total GREATER RECOMP_GEN_PIECE_BUDGET)
      recomp_make_piece(${index} ${sources})
      list(APPEND targets recomp_piece_${index})
      math(EXPR index "${index} + 1")
      set(sources "")
      set(total ${size})
    endif()
    list(APPEND sources "${src}")
    set(bytes ${total})
  endforeach()
  if(sources)
    recomp_make_piece(${index} ${sources})
    list(APPEND targets recomp_piece_${index})
  endif()
  file(GLOB built ${CMAKE_BINARY_DIR}/librecomp_piece_*.dylib)
  foreach(path IN LISTS built)
    get_filename_component(name "${path}" NAME)
    if(name MATCHES "^librecomp_piece_([0-9]+)\\.dylib$" AND CMAKE_MATCH_1 GREATER index)
      file(REMOVE "${path}")
    endif()
  endforeach()
  set(${result} ${targets} PARENT_SCOPE)
endfunction()

if(RECOMP_HAVE_GEN)
  if(APPLE AND NOT IOS AND NOT ANDROID AND RECOMP_SPLIT_TRANSLATION)
    set(RECOMP_AUX_SHARED ON)
  else()
    set(RECOMP_AUX_SHARED OFF)
  endif()
  set(REAL_GEN ${RECOMP_REAL_GEN})
  if(RECOMP_AUX_SHARED AND REAL_GEN)
    recomp_split_main_gen(RECOMP_GEN_PIECES)
    set(RECOMP_GEN_SOURCES ${RECOMP_GEN_DIR}/table.c ${RECOMP_GEN_DIR}/call_facts.c)
  else()
    recomp_translation_sources(RECOMP_GEN_SOURCES "${RECOMP_GEN_DIR}")
  endif()
  add_library(recomp_gen STATIC ${RECOMP_GEN_SOURCES})
  set_target_properties(recomp_gen PROPERTIES
    ARCHIVE_OUTPUT_DIRECTORY ${RECOMP_ARCHIVE_DIR} OUTPUT_NAME recomp_gen)
  # -I<gen> for x86.h beside the sources, -I<root> for the canonical copy,
  # -I<runtime> for intrinsics.h.
  target_include_directories(recomp_gen PRIVATE ${RECOMP_GEN_DIR} ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime)
  target_include_directories(recomp_gen INTERFACE ${RECOMP_GEN_DIR})
  target_compile_options(recomp_gen PRIVATE ${RECOMP_WARN_GEN})
  recomp_generated_source_options(${RECOMP_GEN_SOURCES})
  recomp_translation_overrides(recomp_gen "${RECOMP_GEN_DIR}" "${RECOMP_EFFECTIVE_OVERRIDE}")
  recomp_optimize(recomp_gen 2)
  # Desktop macOS keeps each auxiliary image in its own dylib: one static
  # executable of every translation exceeds the arm64 branch range. Other
  # platforms keep the static whole-archive layout.
  set(RECOMP_GEN_AUX_TARGETS "")
  foreach(key ${RECOMP_AUX_MODULES})
    set(dir "${RECOMP_GEN_DIR}/aux-${key}")
    if(NOT EXISTS "${dir}/table.c")
      message(FATAL_ERROR "Missing auxiliary translation: ${dir}")
    endif()
    set(aux_target "recomp_gen_${key}")
    recomp_translation_sources(aux_sources "${dir}")
    # The main image owns the one call-fact table the runtime links against.
    list(FILTER aux_sources EXCLUDE REGEX "/call_facts\\.c$")
    if(RECOMP_AUX_SHARED)
      add_library(${aux_target} SHARED ${aux_sources})
      set_target_properties(${aux_target} PROPERTIES
        INSTALL_NAME_DIR "@rpath" OUTPUT_NAME ${aux_target})
      # The runtime and the main table stay in the app; resolve their symbols
      # from the main image at load time instead of linking a second copy.
      target_link_options(${aux_target} PRIVATE "-Wl,-undefined,dynamic_lookup")
    else()
      add_library(${aux_target} STATIC ${aux_sources})
      set_target_properties(${aux_target} PROPERTIES
        ARCHIVE_OUTPUT_DIRECTORY ${RECOMP_ARCHIVE_DIR} OUTPUT_NAME ${aux_target})
    endif()
    target_include_directories(${aux_target} PRIVATE ${dir} ${RECOMP_GEN_DIR} ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime)
    target_compile_options(${aux_target} PRIVATE ${RECOMP_WARN_GEN})
    recomp_optimize(${aux_target} 2)
    list(APPEND RECOMP_GEN_AUX_TARGETS ${aux_target})
  endforeach()
  # The game's native replacements (game.toml [translate] native).
  if(RECOMP_NATIVE_HEADER AND NOT RECOMP_TRANSLATE STREQUAL "STUB")
    target_sources(recomp_gen PRIVATE ${RECOMP_NATIVE_SOURCES})
  endif()
endif()

# The portable spelling of -Wl,-force_load: every generated object is kept
# whether or not anything references it, because the dispatch table is
# reached by address.
function(recomp_link_gen target)
  target_link_libraries(${target} PRIVATE "$<LINK_LIBRARY:WHOLE_ARCHIVE,recomp_gen>")
  foreach(piece ${RECOMP_GEN_PIECES})
    target_link_libraries(${target} PRIVATE ${piece})
  endforeach()
  target_include_directories(${target} PRIVATE ${RECOMP_GEN_DIR})
endfunction()

function(recomp_link_aux target)
  foreach(aux_target ${RECOMP_GEN_AUX_TARGETS})
    if(RECOMP_AUX_SHARED)
      target_link_libraries(${target} PRIVATE ${aux_target})
    else()
      target_link_libraries(${target} PRIVATE "$<LINK_LIBRARY:WHOLE_ARCHIVE,${aux_target}>")
    endif()
  endforeach()
endfunction()
