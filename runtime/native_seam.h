/* native_seam.h - services a game's native overrides may call.
 *
 * A game's [translate] overrides header is compiled into the translation as C,
 * so everything here has C linkage and C types. None of it names a game.
 */
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
/* Same typedef as x86.h; repeating it is valid in C11 and C++. */
typedef struct X86 X86;
#ifdef __cplusplus
extern "C" {
#endif

/* The size, in points, of the display the program's window is shown on - the
 * area a fullscreen window fills. Non-zero with both set, or zero
 * when the host has no display to report. Safe from any guest thread. */
int host_display_screen_size(int *w, int *h);

/* The host path a guest file would be written to in the writable overlay tier
 * (the player's profile). Zero, with nothing written, when there is no such
 * tier: a native override must never write into the game's own directory. */
int recomp_writable_path(const char *guest_path, char *out, size_t out_len);

/* The host path a guest file is read from, through every overlay tier and
 * then the game directory. Zero when the path cannot be resolved. */
int recomp_readable_path(const char *guest_path, char *out, size_t out_len);

/* Reserve `size` bytes in the guest heap and return a guest address, or 0.
 * The block is 16-byte aligned and zero filled; safe from any guest thread. */
uint32_t recomp_guest_alloc(uint32_t size);
/* Release a block returned by recomp_guest_alloc. */
void recomp_guest_free(uint32_t addr);

/* Alias a synthetic guest path to a host file for READS only. The resolver
 * answers `guest_path` with `host_path` ahead of the overlay and the game
 * directory, so a generated file can be handed to the original loader without
 * writing anything into the game directory. Returns non-zero on success.
 *
 * LIFE CYCLE. An alias lives until recomp_file_alias_remove or process exit;
 * it is never cleared by an overlay reset, so the caller owns removal and must
 * keep the host file readable while the alias exists. The path is normalised
 * like any other guest path (drive/root stripped, case-insensitive matches),
 * writes and deletes ignore aliases entirely, and adding the same guest path
 * again replaces the old target. */
int recomp_file_alias_add(const char *guest_path, const char *host_path);
/* Drop an alias added by recomp_file_alias_add; a missing alias is a no-op. */
void recomp_file_alias_remove(const char *guest_path);

/* Create a unique host file under the platform temp directory, write `len`
 * bytes and close it. `suffix` is appended verbatim to the generated name
 * (pass its leading dot). `out` receives the path, which the caller owns and
 * removes when done; on any failure `out` is an empty string and no file is
 * left behind. `data` may be null only when `len` is zero. Returns non-zero on
 * success. */
int recomp_temp_file(const char *suffix, const void *data, size_t len, char *out, size_t out_len);

/* Offer a DirectDraw display mode, as the host does for its own modes. */
int ddraw_add_mode(int w, int h, int bpp);

int recomp_registry_dword(const char *key, const char *name, uint32_t *value);
void recomp_registry_set_dword(const char *key, const char *name, uint32_t value);
bool ddraw_enum_display_mode(uint32_t index, uint32_t *w, uint32_t *h, uint32_t *bpp);

/* The guest has finished drawing its 3D scene and is about to draw its overlay.
 * Mode 0 only marks the boundary (a draw trace prints it); mode 1 filters the
 * scene with FXAA first, on the host, so overlay draws stay unfiltered. Needs
 * the wgpu renderer: any non-zero mode without it, or with an unusable device
 * state, stops with a diagnostic. Returns zero on success. Requires the guest
 * scheduler baton. */
int d3d8_scene_boundary(X86 *c, uint32_t mode);

#ifdef __cplusplus
}
#endif
