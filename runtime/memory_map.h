#ifndef RECOMP_MEMORY_MAP_H
#define RECOMP_MEMORY_MAP_H

#ifndef GUEST_SIZE
#define GUEST_SIZE 0x10000000u /* 256 MB arena; RECOMP_GUEST_SIZE grows it for auxiliary modules */
#endif

/* Guest memory layout (see plan Global constraints, as corrected):
 *   0x00400000 + SizeOfImage  image (preferred base, no relocation).  The end
 *                             is a PE header value, not a constant; ask the
 *                             loader (loader_image_limit()) for it.
 *   GUEST_HEAP_BASE..GUEST_HEAP_END  heap arena (configurable)
 *   0x0f000000                stack top, grows down (8 MB)
 *   0x0f000000..0x0fd00000    mod heap (mods/guest_mem.cpp)
 *   0x0fe00000                TEB (FS base)
 *   0x0ff00000 + 16*i         import shim trampoline for import i
 */
/* The build passes the game's image base; this default serves the test harness. */
#ifndef GUEST_IMAGE_BASE
#define GUEST_IMAGE_BASE 0x00400000u
#endif
/* The build passes the game's heap start ([game] heap_base); the default
 * serves the test harness and games whose image ends below 16 MB. */
#ifndef GUEST_HEAP_BASE
#define GUEST_HEAP_BASE 0x01000000u
#endif
/* Larger game heaps can sit above the fixed low runtime regions. */
#ifndef GUEST_HEAP_END
#define GUEST_HEAP_END 0x0e000000u
#endif
#if GUEST_HEAP_BASE >= GUEST_HEAP_END || GUEST_HEAP_END > GUEST_SIZE
#error "Guest heap must be a nonempty interval inside the arena"
#endif
#if GUEST_HEAP_BASE < 0x10000000u && GUEST_HEAP_END > 0x0e000000u
#error "Guest heap overlaps the mod heap, stack, TEB or import regions"
#endif
#define GUEST_STACK_TOP 0x0f000000u
#define GUEST_TEB_BASE 0x0fe00000u
#define GUEST_SHIM_BASE 0x0ff00000u
#define GUEST_SHIM_END 0x10000000u
#define GUEST_SHIM_STRIDE 16u
#define GUEST_RETURN_SENTINEL 0x0fdfff00u
#define GUEST_NULL_LIMIT 0x10000u

#endif
