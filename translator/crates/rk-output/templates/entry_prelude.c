void recomp_enter(X86 *c, uint32_t index);
void recomp_enter_raw(X86 *c, uint32_t index, void (*raw)(X86 *));
#if RECOMP_CALL_HOOKS
#define RECOMP_SLOW_(i) RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow[i], __ATOMIC_RELAXED))
#else
#define RECOMP_SLOW_(i) RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow_routes, __ATOMIC_RELAXED))
#endif
#define RECOMP_ENTER(i, base) \
    do { recomp_execution_checkpoint(c); if (RECOMP_SLOW_(i)) recomp_enter(c, i); else base; } while (0)
#define RECOMP_CALL(i, raw) \
    do { recomp_execution_checkpoint(c); if (RECOMP_SLOW_(i)) recomp_enter_raw(c, i, raw); else raw(c); } while (0)
