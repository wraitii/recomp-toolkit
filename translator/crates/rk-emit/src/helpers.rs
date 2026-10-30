use crate::model::Preludes;

pub const RETURN_HELPERS: &str = "\
RECOMP_COLD void recomp_return_slow_(X86 *c) { recomp_return(c); }
static inline void recomp_return_fast_(X86 *c) {
    if (RECOMP_UNLIKELY(c->eip == GUEST_RETURN_SENTINEL)) {
        recomp_return_slow_(c);
        return;
    }
    if (c->dynamic_return_count &&
        c->dynamic_returns[c->dynamic_return_count - 1] == c->eip) {
        --c->dynamic_return_count;
        return;
    }
    if (RECOMP_UNLIKELY(!recomp_is_call_return(c->eip)))
        recomp_return_slow_(c);
}
";

const TABLE_RETURN_CHECK: &str = "\
extern const uint32_t recomp_call_return_min, recomp_call_return_span;
extern const uint32_t recomp_call_return_bits[];
RECOMP_HOT_INLINE int recomp_returns_here_(uint32_t target) {
    uint32_t off = target - recomp_call_return_min;
    return off < recomp_call_return_span && ((recomp_call_return_bits[off >> 5] >> (off & 31)) & 1u);
}
";

pub const MEMORY_HELPERS: &str = "\
RECOMP_HOT_INLINE uint8_t recomp_rd8_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rd8(a);
#else
    uint8_t v;
    RECOMP_BITCOPY(&v, arena + a, 1);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wr8_(uint8_t *arena, uint32_t a, uint8_t v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wr8(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 1);
#endif
}
RECOMP_HOT_INLINE uint16_t recomp_rd16_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rd16(a);
#else
    uint16_t v;
    RECOMP_BITCOPY(&v, arena + a, 2);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wr16_(uint8_t *arena, uint32_t a, uint16_t v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wr16(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 2);
#endif
}
RECOMP_HOT_INLINE uint32_t recomp_rd32_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rd32(a);
#else
    uint32_t v;
    RECOMP_BITCOPY(&v, arena + a, 4);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wr32_(uint8_t *arena, uint32_t a, uint32_t v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wr32(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 4);
#endif
}
RECOMP_HOT_INLINE uint64_t recomp_rd64_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rd64(a);
#else
    uint64_t v;
    RECOMP_BITCOPY(&v, arena + a, 8);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wr64_(uint8_t *arena, uint32_t a, uint64_t v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wr64(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 8);
#endif
}
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS) || \\
    !(defined(__GNUC__) || defined(__clang__))
RECOMP_HOT_INLINE void recomp_rd128_(uint8_t *arena, uint32_t a, uint32_t *v0, uint32_t *v1,
                                     uint32_t *v2, uint32_t *v3) {
    *v0 = recomp_rd32_(arena, a);
    *v1 = recomp_rd32_(arena, a + 4u);
    *v2 = recomp_rd32_(arena, a + 8u);
    *v3 = recomp_rd32_(arena, a + 12u);
}
RECOMP_HOT_INLINE void recomp_wr128_(uint8_t *arena, uint32_t a, uint32_t v0, uint32_t v1,
                                     uint32_t v2, uint32_t v3) {
    recomp_wr32_(arena, a, v0);
    recomp_wr32_(arena, a + 4u, v1);
    recomp_wr32_(arena, a + 8u, v2);
    recomp_wr32_(arena, a + 12u, v3);
}
#else
typedef uint32_t recomp_u32x4_ __attribute__((vector_size(16)));
RECOMP_HOT_INLINE void recomp_rd128_(uint8_t *arena, uint32_t a, uint32_t *v0, uint32_t *v1,
                                     uint32_t *v2, uint32_t *v3) {
    recomp_u32x4_ v;
    RECOMP_BITCOPY(&v, arena + a, 16);
    *v0 = v[0];
    *v1 = v[1];
    *v2 = v[2];
    *v3 = v[3];
}
RECOMP_HOT_INLINE void recomp_wr128_(uint8_t *arena, uint32_t a, uint32_t v0, uint32_t v1,
                                     uint32_t v2, uint32_t v3) {
    recomp_u32x4_ v = {v0, v1, v2, v3};
    RECOMP_BITCOPY(arena + a, &v, 16);
}
#endif
RECOMP_HOT_INLINE float recomp_rdf32_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rdf32(a);
#else
    float v;
    RECOMP_BITCOPY(&v, arena + a, 4);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wrf32_(uint8_t *arena, uint32_t a, float v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wrf32(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 4);
#endif
}
RECOMP_HOT_INLINE double recomp_rdf64_(uint8_t *arena, uint32_t a) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    return rdf64(a);
#else
    double v;
    RECOMP_BITCOPY(&v, arena + a, 8);
    return v;
#endif
}
RECOMP_HOT_INLINE void recomp_wrf64_(uint8_t *arena, uint32_t a, double v) {
#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)
    (void)arena;
    wrf64(a, v);
#else
    RECOMP_BITCOPY(arena + a, &v, 8);
#endif
}
";

pub const FLOAT_HELPERS: &str = "\
RECOMP_HOT_INLINE void recomp_fcom_(uint16_t *status, double a, double b, int quiet) {
    if (RECOMP_UNLIKELY(isnan(a) || isnan(b))) {
        fcom_status(status, a, b, quiet);
    } else {
        *status = (uint16_t)((*status & (uint16_t)~0x4700u) |
                            ((uint16_t)(a < b) << 8) | ((uint16_t)(a == b) << 14));
    }
}
";

pub fn preludes() -> Preludes {
    Preludes {
        memory_helpers: MEMORY_HELPERS.to_string(),
        return_helpers: RETURN_HELPERS.to_string(),
        table_return_helpers: TABLE_RETURN_CHECK.to_string()
            + &RETURN_HELPERS.replace("recomp_is_call_return(", "recomp_returns_here_("),
        float_helpers: FLOAT_HELPERS.to_string(),
    }
}
