# Each executable lists exactly the host sources its shell script listed, so
# which strong host_* callbacks a binary carries does not change.
set(HOST ${CMAKE_CURRENT_SOURCE_DIR})

# Portable host code shared by every host that boots the guest.
add_library(host_common OBJECT boot.cpp report_lock.cpp)
target_include_directories(host_common PUBLIC ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime)
target_link_libraries(host_common PUBLIC recomp_runtime)
target_compile_options(host_common PRIVATE ${RECOMP_WARN_HOST})
recomp_optimize(host_common 1)

# The launcher's portable core (host/launcher/): finding, checking and
# importing the player's game. miniz reads and writes ZIPs.
add_library(recomp_miniz OBJECT ${RECOMP_ROOT}/third_party/miniz/miniz.c)
target_include_directories(recomp_miniz PUBLIC ${RECOMP_ROOT}/third_party/miniz)
set_target_properties(recomp_miniz PROPERTIES POSITION_INDEPENDENT_CODE ON)
recomp_optimize(recomp_miniz 2)
add_library(launcher_core OBJECT ${HOST}/launcher/launcher.cpp ${HOST}/launcher/launcher_canvas.cpp
                                 ${HOST}/launcher/launcher_ui.cpp)
target_include_directories(launcher_core PUBLIC ${RECOMP_ROOT})
target_compile_options(launcher_core PRIVATE ${RECOMP_WARN_STRICT})
set_target_properties(launcher_core PROPERTIES POSITION_INDEPENDENT_CODE ON)
recomp_optimize(launcher_core 1)

# Compiling Objective-C++ under ARC, as every script did; a no-op for C++.
if(APPLE)
  set(RECOMP_OBJC_ARC $<$<COMPILE_LANGUAGE:OBJCXX>:-fobjc-arc>)
else()
  set(RECOMP_OBJC_ARC "")
endif()

add_subdirectory(gpu)
include(${RECOMP_ROOT}/cmake/RustCompositor.cmake)

if(NOT IOS AND NOT ANDROID)
# Portable pieces built everywhere. compositor_tests needs a real device for
# its pixel half (label gpu); ui_layer_tests needs nothing.
add_executable(compositor_tests ${HOST}/compositor.cpp ${HOST}/tests/compositor_tests.cpp)
target_include_directories(compositor_tests PRIVATE ${RECOMP_ROOT} ${HOST})
target_compile_options(compositor_tests PRIVATE ${RECOMP_WARN_WERROR})
target_link_libraries(compositor_tests PRIVATE gpu_backend recomp_compositor)
recomp_optimize(compositor_tests 1)
recomp_test_binary(compositor_tests)
add_test(NAME compositor_tests COMMAND compositor_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(compositor_tests PROPERTIES LABELS gpu)

add_executable(input_script_tests ${HOST}/input_script.cpp ${HOST}/script.cpp ${HOST}/tests/input_script_tests.cpp)
target_include_directories(input_script_tests PRIVATE ${RECOMP_ROOT} ${CMAKE_BINARY_DIR}/generated)
target_compile_options(input_script_tests PRIVATE ${RECOMP_WARN_WERROR})
recomp_test_binary(input_script_tests)
add_test(NAME input_script_tests COMMAND input_script_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(input_script_tests PROPERTIES LABELS nogame)

add_executable(input_touch_tests ${HOST}/input_touch.cpp ${HOST}/tests/input_touch_tests.cpp)
target_include_directories(input_touch_tests PRIVATE ${RECOMP_ROOT})
target_compile_options(input_touch_tests PRIVATE ${RECOMP_WARN_WERROR})
recomp_link_sdl(input_touch_tests)
recomp_optimize(input_touch_tests 1)
recomp_test_binary(input_touch_tests)
add_test(NAME input_touch_tests COMMAND input_touch_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(input_touch_tests PROPERTIES LABELS nogame)

add_executable(keypad_tests ${HOST}/keypad_layout.cpp ${HOST}/keypad_modifiers.cpp ${HOST}/tests/keypad_tests.cpp)
target_include_directories(keypad_tests PRIVATE ${RECOMP_ROOT})
target_compile_options(keypad_tests PRIVATE ${RECOMP_WARN_WERROR})
recomp_link_sdl(keypad_tests)
recomp_optimize(keypad_tests 1)
recomp_test_binary(keypad_tests)
add_test(NAME keypad_tests COMMAND keypad_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(keypad_tests PROPERTIES LABELS nogame)

add_executable(controls_tests ${HOST}/controls/json.cpp ${HOST}/controls/layout.cpp
                              ${HOST}/controls/builtin_layouts.cpp ${HOST}/controls/layout_store.cpp
                              ${HOST}/controls/router.cpp ${HOST}/controls/vpad.cpp
                              ${HOST}/controls/overlay_view.cpp
                              ${HOST}/controls/raster.cpp ${HOST}/controls/overlay_paint.cpp
                              ${HOST}/controls/pad_art.cpp
                              ${HOST}/controls/haptics.cpp
                              ${HOST}/controls/editor.cpp
                              ${HOST}/controls/editor_actions.cpp
                              ${HOST}/keypad_layout.cpp
                              ${HOST}/keypad_modifiers.cpp ${HOST}/tests/controls_tests.cpp
                              ${HOST}/tests/keypad_legacy_oracle.cpp ${RECOMP_ROOT}/mods/font6x8.cpp)
target_include_directories(controls_tests PRIVATE ${RECOMP_ROOT})
target_compile_options(controls_tests PRIVATE ${RECOMP_WARN_WERROR})
target_link_libraries(controls_tests PRIVATE recomp_platform)
recomp_link_sdl(controls_tests)
recomp_optimize(controls_tests 1)
recomp_test_binary(controls_tests)
get_target_property(CONTROLLER_PREVIEW_SOURCES controls_tests SOURCES)
list(FILTER CONTROLLER_PREVIEW_SOURCES EXCLUDE REGEX "/tests/")
add_executable(controller_preview EXCLUDE_FROM_ALL ${CONTROLLER_PREVIEW_SOURCES}
               ${HOST}/controls/pad_mapper.cpp ${HOST}/controls/pad_mapper_editor.cpp
               ${RECOMP_ROOT}/tools/controller_preview.cpp)
target_include_directories(controller_preview PRIVATE ${RECOMP_ROOT})
target_compile_options(controller_preview PRIVATE ${RECOMP_WARN_WERROR})
target_link_libraries(controller_preview PRIVATE recomp_platform)
recomp_link_sdl(controller_preview)
recomp_optimize(controller_preview 1)

add_test(NAME controls_tests COMMAND controls_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(controls_tests PROPERTIES LABELS nogame)

add_executable(launcher_tests ${HOST}/tests/launcher_tests.cpp ${RECOMP_ROOT}/mods/font6x8.cpp)
target_include_directories(launcher_tests PRIVATE ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime ${RECOMP_ROOT}/third_party/lua
                                                  ${CMAKE_BINARY_DIR}/generated)
target_compile_options(launcher_tests PRIVATE ${RECOMP_WARN_WERROR})
if(WIN32)
  # fopen and getenv, as the POSIX hosts use them.
  target_compile_definitions(launcher_tests PRIVATE _CRT_SECURE_NO_WARNINGS)
endif()
target_link_libraries(launcher_tests PRIVATE launcher_core recomp_miniz recomp_platform)
recomp_optimize(launcher_tests 1)
recomp_test_binary(launcher_tests)
add_test(NAME launcher_tests COMMAND launcher_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(launcher_tests PROPERTIES LABELS nogame)

add_executable(ui_layer_tests ${HOST}/ui_layer.cpp ${HOST}/tests/test_frame_builder.cpp ${HOST}/tests/ui_layer_tests.cpp)
target_compile_options(ui_layer_tests PRIVATE ${RECOMP_WARN_WERROR})
recomp_optimize(ui_layer_tests 1)
recomp_test_binary(ui_layer_tests)
add_test(NAME ui_layer_tests COMMAND ui_layer_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(ui_layer_tests PROPERTIES LABELS nogame)
endif()

# The hosts and the full host suite build on every platform over gpu_backend.
if(APPLE)
  set(FW_FOUNDATION "-framework Foundation")
  set(FW_METAL "-framework Metal" "-framework MetalKit" "-framework CoreText"
               "-framework CoreVideo" "-framework CoreGraphics" "-framework QuartzCore")
else()
  set(FW_FOUNDATION "")
  set(FW_METAL "")
endif()

# recomp_host(<target> [GEN] [UI_LAYER] [OPT <n>] SOURCES ... LIBS ... FRAMEWORKS ...):
# a host or a test binary over the runtime, shims, mods and Lua.
function(recomp_host target)
  cmake_parse_arguments(ARG "GEN;UI_LAYER" "OPT" "SOURCES;FRAMEWORKS;LIBS" ${ARGN})
  if(ANDROID AND target STREQUAL "recomp_app")
    add_library(${target} SHARED ${ARG_SOURCES})
  else()
    add_executable(${target} ${ARG_SOURCES})
  endif()
  target_link_libraries(${target} PRIVATE ${ARG_LIBS} ${ARG_FRAMEWORKS})
  target_include_directories(${target} PRIVATE ${RECOMP_ROOT} ${RECOMP_ROOT}/runtime
                                               ${RECOMP_ROOT}/host ${RECOMP_ROOT}/third_party/lua
                                               ${CMAKE_BINARY_DIR}/generated)
  target_compile_options(${target} PRIVATE ${RECOMP_WARN_HOST} ${RECOMP_OBJC_ARC})
  if(ARG_UI_LAYER)
    target_compile_definitions(${target} PRIVATE RECOMP_PRESENT_HAS_UI_LAYER=1)
  endif()
  if(NOT ARG_OPT)
    set(ARG_OPT 1)
  endif()
  recomp_optimize(${target} ${ARG_OPT})
  if(ARG_GEN)
    recomp_link_gen(${target})
    recomp_link_aux(${target})
  endif()
endfunction()

# The mixer, its output device and the synth: portable, over SDL audio.
set(RECOMP_AUDIO_SOURCES ${HOST}/audio/mixer.cpp ${HOST}/audio/sdl_sink.cpp
                      ${HOST}/audio_math.cpp ${HOST}/audio_capture.cpp)
if(RECOMP_AUDIO_MIDI)
  list(APPEND RECOMP_AUDIO_SOURCES ${HOST}/audio/midi_synth.cpp)
endif()
set(RECOMP_CORE_LIBS recomp_platform recomp_runtime recomp_dx recomp_mods lua host_common gpu_backend recomp_compositor)
# The on-screen controls the presenter draws (controls_host.cpp is the app's own).
set(RECOMP_CONTROLS_SOURCES ${HOST}/controls/layout.cpp
                         ${HOST}/controls/builtin_layouts.cpp ${HOST}/controls/layout_store.cpp
                         ${HOST}/controls/router.cpp ${HOST}/controls/vpad.cpp
                         ${HOST}/controls/overlay_view.cpp
                         ${HOST}/controls/overlay.cpp ${HOST}/controls/raster.cpp
                          ${HOST}/controls/overlay_paint.cpp ${HOST}/controls/pad_art.cpp
                          ${HOST}/controls/haptics.cpp
                          ${HOST}/controls/editor.cpp ${HOST}/controls/editor_actions.cpp
                         ${HOST}/controls/pad_mapper.cpp ${HOST}/controls/pad_mapper_editor.cpp
                         ${HOST}/keypad_layout.cpp ${HOST}/keypad_modifiers.cpp)
set(RECOMP_PRESENT_SOURCES ${HOST}/present_pixels.cpp ${HOST}/present_thread.cpp ${HOST}/compositor.cpp
                        ${HOST}/ui_layer.cpp ${HOST}/performance_overlay.cpp ${RECOMP_CONTROLS_SOURCES}
                        ${HOST}/d3d_render.cpp
                        ${HOST}/indexed_frame.cpp)
# The host's platform file: desktop or iPadOS (host/sdl/platform_ui.h), and
# the launcher's (host/launcher/launcher_sdl.h; iPadOS keeps its own in
# platform_ui_ios.mm).
set(RECOMP_LAUNCHER_SOURCES $<TARGET_OBJECTS:launcher_core> $<TARGET_OBJECTS:recomp_miniz>
                         ${HOST}/launcher/launcher_config.cpp ${HOST}/launcher/launcher_sdl.cpp
                         ${HOST}/launcher/configure_ui.cpp ${HOST}/preferences.cpp)
if(IOS)
  set(RECOMP_PLATFORM_UI ${HOST}/sdl/platform_ui_ios.mm)
  list(APPEND FW_METAL "-framework UIKit" "-framework UniformTypeIdentifiers" "-framework CoreHaptics")
elseif(ANDROID)
  set(RECOMP_PLATFORM_UI ${HOST}/sdl/platform_ui_desktop.cpp ${HOST}/launcher/launcher_platform_android.cpp)
else()
  set(RECOMP_PLATFORM_UI ${HOST}/sdl/platform_ui_desktop.cpp ${HOST}/launcher/launcher_platform_desktop.cpp)
endif()

if(TARGET recomp_gen)
  if(NOT IOS AND NOT ANDROID)
  recomp_host(recomp_headless GEN
    SOURCES ${HOST}/headless_main.cpp ${HOST}/input.cpp ${HOST}/input_gate.cpp ${HOST}/compositor.cpp
            ${HOST}/script.cpp ${HOST}/input_script.cpp ${HOST}/input_script_host.cpp ${HOST}/page_overlay.cpp ${RECOMP_AUDIO_SOURCES}
    LIBS ${RECOMP_CORE_LIBS}
    FRAMEWORKS ${FW_FOUNDATION})
  recomp_link_sdl(recomp_headless)
  if(TARGET core_mods)
    add_dependencies(recomp_headless core_mods)
  endif()

  recomp_host(recomp_smoke GEN UI_LAYER
    SOURCES ${HOST}/smoke_main.cpp ${HOST}/script.cpp ${HOST}/input.cpp ${HOST}/input_gate.cpp
            ${HOST}/input_touch.cpp ${HOST}/controls/vpad_host_api.cpp ${HOST}/sdl/keymap.cpp
            ${HOST}/page_overlay.cpp ${RECOMP_PRESENT_SOURCES} ${HOST}/audio_math.cpp
    LIBS ${RECOMP_CORE_LIBS}
    FRAMEWORKS ${FW_FOUNDATION} ${FW_METAL})
  recomp_link_sdl(recomp_smoke)
  if(TARGET core_mods)
    add_dependencies(recomp_smoke core_mods)
  endif()

  endif()

  recomp_host(recomp_app GEN UI_LAYER OPT 2
    SOURCES ${HOST}/input_script.cpp ${HOST}/input_script_host.cpp ${HOST}/script.cpp
            ${HOST}/sdl/main.cpp ${HOST}/sdl/keymap.cpp ${RECOMP_PLATFORM_UI} ${RECOMP_LAUNCHER_SOURCES}
            ${HOST}/controls/controls_host.cpp ${HOST}/controls/gamepad_sdl.cpp
            ${HOST}/controls/vpad_host_api.cpp
            ${HOST}/game_path.cpp ${HOST}/present.cpp
            ${HOST}/page_overlay.cpp
            ${RECOMP_PRESENT_SOURCES}
            ${HOST}/input.cpp ${HOST}/input_gate.cpp ${HOST}/input_touch.cpp ${RECOMP_AUDIO_SOURCES}
    LIBS ${RECOMP_CORE_LIBS}
    FRAMEWORKS ${FW_METAL})
  recomp_link_sdl(recomp_app)
  set_target_properties(recomp_app PROPERTIES OUTPUT_NAME ${RECOMP_APP_NAME})
  if(APPLE AND NOT IOS)
    add_executable(configure_launcher ${HOST}/configure_launcher.cpp ${HOST}/controls/json.cpp)
    target_link_libraries(configure_launcher PRIVATE recomp_platform)
    target_include_directories(configure_launcher PRIVATE ${RECOMP_ROOT})
    target_compile_definitions(configure_launcher PRIVATE RECOMP_APP_NAME="${RECOMP_APP_NAME}")
    add_dependencies(recomp_app configure_launcher)
    set_target_properties(configure_launcher PROPERTIES OUTPUT_NAME ConfigureLauncher)
  endif()
  if(ANDROID)
    # SDLActivity loads libmain.so from the APK's native library directory.
    set_target_properties(recomp_app PROPERTIES OUTPUT_NAME main)
    target_link_libraries(recomp_app PRIVATE log)
    # Core adaptations must be available before guest startup on mobile too.
    # Their manifests are staged into APK assets by tools/build.py.
    recomp_add_builtin_mods(recomp_app)
  elseif(IOS)
    # No plugin loading on a stock device: the core mods are compiled in.
    recomp_add_builtin_mods(recomp_app RESOURCES $<TARGET_BUNDLE_CONTENT_DIR:recomp_app>)
    recomp_ios_bundle(recomp_app)
  elseif(EMSCRIPTEN)
    # The web: core mods compiled in, the host's resources packaged at
    # /app/resources (runtime/layout.cpp), threads for the game.
    recomp_add_builtin_mods(recomp_app)
    set(web_resources ${RECOMP_OUT}/web-resources)
    file(REMOVE_RECURSE ${web_resources})
    file(MAKE_DIRECTORY ${web_resources})
    if(EXISTS ${RECOMP_BUILD_ROOT}/recomp/symbols.json)
      file(COPY ${RECOMP_BUILD_ROOT}/recomp/symbols.json DESTINATION ${web_resources})
    endif()
    if(EXISTS ${RECOMP_GAME_DIR}/mods/core)
      file(COPY ${RECOMP_GAME_DIR}/mods/core DESTINATION ${web_resources}/mods FILES_MATCHING PATTERN "*.toml"
           PATTERN "*.json" PATTERN "*.lua" PATTERN "*.png" PATTERN "*.bmp" PATTERN "*.txt")
    endif()
    # The player page (tools/web_launcher.py --web-build places it beside the launcher).
    set(APP ${RECOMP_APP_NAME})
    set(TITLE ${RECOMP_GAME_NAME})
    configure_file(${RECOMP_ROOT}/web/player/index.html ${RECOMP_OUT}/index.html @ONLY)
    target_link_options(recomp_app PRIVATE
      -pthread -sPTHREAD_POOL_SIZE=24 -sWASMFS
      -sINITIAL_MEMORY=1073741824 -sALLOW_MEMORY_GROWTH=1 -sMAXIMUM_MEMORY=4294967296
      -sSTACK_SIZE=8388608 -sDEFAULT_PTHREAD_STACK_SIZE=16777216
      -sENVIRONMENT=web,worker -sEXIT_RUNTIME=0 -sINVOKE_RUN=0
      -sEXPORTED_RUNTIME_METHODS=ENV,callMain,FS
      --preload-file ${web_resources}@/app/resources)
  elseif(APPLE)
    recomp_mac_bundle(recomp_app)
  else()
    # Desktop packages carry manifests beside statically linked core plugins.
    # This avoids shipping a plugin compiled for a different host platform.
    recomp_add_builtin_mods(recomp_app RESOURCES $<TARGET_FILE_DIR:recomp_app>/resources)
    if(CMAKE_SYSTEM_NAME STREQUAL "Linux" AND (RECOMP_VIDEO OR RECOMP_PCODE_LIB))
      # Keep staged FFmpeg and p-code libraries discoverable beside the executable.
      set_target_properties(recomp_app PROPERTIES BUILD_RPATH "$ORIGIN" INSTALL_RPATH "$ORIGIN")
    endif()
  endif()

endif()

# Host tests: offscreen GPU work and the mixer rendered offline, no window and
# no audio device. Labelled `gpu`: the renderer tests need a device.
# report_lock.cpp is a source here rather than host_common, because the script
# never linked boot.cpp into this suite.
if(NOT IOS AND NOT ANDROID)
recomp_host(host_tests UI_LAYER
  SOURCES ${HOST}/tests/host_tests.cpp ${HOST}/tests/test_frame_builder.cpp
          ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp
          ${RECOMP_ROOT}/runtime/tests/stub_dx_report_failure.cpp
          ${HOST}/report_lock.cpp ${HOST}/script.cpp ${HOST}/present.cpp
          ${RECOMP_ROOT}/mods/font6x8.cpp
          ${HOST}/game_path.cpp
          ${HOST}/page_overlay.cpp ${RECOMP_PRESENT_SOURCES} ${HOST}/input.cpp ${HOST}/input_gate.cpp
          ${RECOMP_AUDIO_SOURCES}
          $<$<NOT:$<BOOL:${RECOMP_AUDIO_MIDI}>>:${HOST}/audio/midi_synth.cpp>
          ${RECOMP_ROOT}/dx/com.cpp ${RECOMP_ROOT}/dx/host_api.cpp
          ${RECOMP_ROOT}/dx/display_stubs.cpp ${RECOMP_ROOT}/dx/dinput.cpp
          ${RECOMP_ROOT}/dx/dinput_joystick.cpp
          ${RECOMP_ROOT}/dx/d3d11.cpp ${RECOMP_ROOT}/dx/dxgi.cpp
          ${RECOMP_ROOT}/dx/d3dcompiler.cpp ${RECOMP_ROOT}/dx/d3dx10.cpp
  LIBS recomp_platform recomp_runtime gpu_backend gpu_fake recomp_compositor
  FRAMEWORKS ${FW_FOUNDATION} ${FW_METAL})
recomp_link_sdl(host_tests)
recomp_test_binary(host_tests)
add_test(NAME host_tests COMMAND host_tests WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(host_tests PROPERTIES LABELS gpu)
endif()

# The production headless capture path with synthetic guest windows and no GPU.
if(NOT IOS AND NOT ANDROID)
recomp_host(headless_tests
  SOURCES ${HOST}/tests/headless_tests.cpp ${RECOMP_ROOT}/runtime/tests/stub_recomp_call.cpp
          ${HOST}/input.cpp ${HOST}/input_gate.cpp ${HOST}/compositor.cpp ${HOST}/script.cpp
          ${HOST}/input_script.cpp ${HOST}/input_script_host.cpp
          ${HOST}/page_overlay.cpp ${RECOMP_AUDIO_SOURCES}
  LIBS recomp_platform recomp_runtime recomp_dx host_common recomp_compositor
  FRAMEWORKS ${FW_FOUNDATION})
recomp_link_sdl(headless_tests)
recomp_test_binary(headless_tests)
add_test(NAME headless_tests COMMAND headless_tests ${CMAKE_CURRENT_BINARY_DIR}/capture-frames
  WORKING_DIRECTORY ${RECOMP_ROOT})
set_tests_properties(headless_tests PROPERTIES LABELS nogame)
endif()
