// gamepad_sdl.h - the first physical controller, through SDL_Gamepad: opened
// on hot-plug, read into the shared virtual pad as kPadSourceController, and
// rumbled on request. Called only from the SDL host's thread (main.cpp and
// controls_host.cpp). SDL's standard mapping names DualSense, Xbox and MFi
// buttons alike, so there are no per-controller tables here.
// Design: docs/superpowers/specs/2026-09-17-touch-controls-design.md, 7.2.
#pragma once

#include <SDL3/SDL.h>

#include <cstdint>
#include <string>

namespace controls {

// SDL_EVENT_GAMEPAD_ADDED opens the pad when none is open;
// SDL_EVENT_GAMEPAD_REMOVED of the open one zeros its state (the host mapper
// publishes releases), marks it disconnected, closes it and opens any other
// pad still attached. Button and axis events pass raw values through the
// host mapper at once, so a press and release inside one pump keep their edges.
void gamepad_handle_event(const SDL_Event &e);
// Once per pump, after the events: re-reads the open pad's whole raw state
// (a no-op when nothing changed).
void gamepad_poll();
void gamepad_haptic_service();
void gamepad_haptic_shutdown();
void gamepad_haptic_focus(bool focused);
bool gamepad_connected();
bool gamepad_rumble_supported();
std::string gamepad_name();
// SDL_RumbleGamepad on the open pad; false when there is none or SDL refused.
bool gamepad_rumble(uint16_t low, uint16_t high, uint32_t ms);

} // namespace controls
