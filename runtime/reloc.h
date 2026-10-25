/* reloc.h - table and lookup declarations for heap-loaded relocatable bodies. */
#ifndef RECOMP_RELOC_H
#define RECOMP_RELOC_H

#include "x86.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RecompRelocImport {
    uint32_t slot, target;
} RecompRelocImport;
typedef struct RecompRelocRun {
    uint32_t start, length;
} RecompRelocRun;
typedef struct RecompRelocEntry {
    uint32_t offset;
    void (*fn)(X86 *, uint32_t);
} RecompRelocEntry;
typedef struct RecompRelocTemplate {
    const char *name;
    const uint8_t *code;
    uint32_t code_len;
    const RecompRelocRun *runs;
    uint32_t run_count;
    const RecompRelocImport *imports;
    uint32_t import_count;
    const RecompRelocEntry *entries;
    uint32_t entry_count;
} RecompRelocTemplate;
void recomp_reloc_register(const RecompRelocTemplate *templates, uint32_t count);
int recomp_reloc_cached(X86 *c, uint32_t target);
int recomp_reloc_call(X86 *c, uint32_t target);

#ifdef __cplusplus
}
#endif

#endif
