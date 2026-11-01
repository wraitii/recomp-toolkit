// discovery.h - the code a run found that the translation does not carry.
//
// The code map can miss code: a function only ever reached through a
// pointer nothing resolves, a jump-table slot whose target nothing owns, a
// block Ghidra ended early. Each one shows up at run time as a call or a jump
// the address table cannot deliver.
//
// With RECOMP_DISCOVERY naming a file, every such address is written there as
// evidence for fixing the Ghidra analysis and re-exporting the code map.
#pragma once
#include <stdint.h>
#include <stdio.h>

// Generated table.c is C and calls the first two.
#ifdef __cplusplus
extern "C" {
#endif

// Records `target` as code, named by an instruction in the guest at `from`.
// `kind` is "call" or "jump". Ignored when RECOMP_DISCOVERY is unset.
void discovery_note(const char *kind, uint32_t target, uint32_t from);

// Whether discovery_note records anything in this run.
int discovery_enabled(void);

// Writes the file RECOMP_DISCOVERY names. Registered with atexit() on the
// first note, and called directly on the paths that end the process without
// running atexit handlers.
void discovery_write(void);

// What has been recorded so far, for the run report.
uint32_t discovery_count(void);

#ifdef __cplusplus
}
#endif
