#pragma once
#include "../runtime/memory.h"

struct GuestMemoryCallbacks {
    void *context;
    uint32_t (*validate)(void *, uint32_t, uint32_t);
    uint32_t (*read)(void *, uint32_t, uint8_t *, uint32_t);
    uint32_t (*write)(void *, uint32_t, const uint8_t *, uint32_t);
    uint32_t (*allocate)(void *, uint32_t, uint32_t);
    uint32_t (*free)(void *, uint32_t, uint64_t);
    uint64_t (*generation)(void *);
    uint32_t (*write_words)(void *, uint32_t, const uint32_t *, uint32_t);
};

extern "C" {
inline uint32_t guest_memory_validate(void *, uint32_t addr, uint32_t size) noexcept {
    return g_mem && addr && gm_valid(addr, size);
}

inline uint32_t guest_memory_read(void *context, uint32_t addr, uint8_t *out,
                                  uint32_t size) noexcept {
    if (!guest_memory_validate(context, addr, size))
        return 0;
    if (size)
        memcpy(out, gm_ptr(addr), size);
    return 1;
}

inline uint32_t guest_memory_write(void *context, uint32_t addr, const uint8_t *bytes,
                                   uint32_t size) noexcept {
    if (!guest_memory_validate(context, addr, size))
        return 0;
    if (size)
        memcpy(gm_ptr(addr), bytes, size);
    return 1;
}

inline uint32_t guest_memory_allocate(void *, uint32_t size, uint32_t align) noexcept {
    try {
        if (!g_mem || !size || !align || (align & (align - 1)) || align > HEAP_LIMIT - HEAP_BASE)
            return 0;
        return heap_alloc(size, false, align);
    } catch (...) {
        return 0;
    }
}

inline uint32_t guest_memory_free(void *, uint32_t addr, uint64_t generation) noexcept {
    try {
        if (!g_mem || generation != heap_generation())
            return 0;
        return heap_free(addr);
    } catch (...) {
        return 0;
    }
}

inline uint64_t guest_memory_generation(void *) noexcept {
    return heap_generation();
}

inline uint32_t guest_memory_write_words(void *context, uint32_t addr, const uint32_t *words,
                                         uint32_t count) noexcept {
    try {
        if (count > UINT32_MAX / 4 || !guest_memory_validate(context, addr, count * 4))
            return 0;
        for (uint32_t i = 0; i < count; ++i)
            wr32(addr + i * 4, words[i]);
        return 1;
    } catch (...) {
        return 0;
    }
}
}

inline GuestMemoryCallbacks guest_memory_callbacks() noexcept {
    return {nullptr,
            guest_memory_validate,
            guest_memory_read,
            guest_memory_write,
            guest_memory_allocate,
            guest_memory_free,
            guest_memory_generation,
            guest_memory_write_words};
}
