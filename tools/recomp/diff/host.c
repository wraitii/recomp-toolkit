#include <setjmp.h>
#include <stdint.h>
#include <string.h>

#include "x86.h"

static jmp_buf escape_jb;
static int escape_armed;
static const char *escape_reason;
static uint32_t escape_addr;

void harness_escape(const char *reason, uint32_t addr) {
    if (!escape_armed)
        return;
    escape_armed = 0;
    escape_reason = reason;
    escape_addr = addr;
    longjmp(escape_jb, 1);
}

void recomp_x87_contract_violation(X86 *c, uint32_t addr, uint16_t mask, uint16_t value) {
    (void)c;
    (void)mask;
    (void)value;
    harness_escape("x87 entry contract violation", addr);
}

uint8_t recomp_hooks_ever;
uint8_t recomp_slow_routes;
uint32_t recomp_frame_watch;
uint64_t recomp_import_calls;

void diff_set_hooks_ever(uint8_t value) {
    recomp_hooks_ever = value;
    recomp_slow_routes = value;
}

int interp_call(X86 *c, uint32_t target) {
    (void)c;
    harness_escape("interp", target);
    return 0;
}
int recomp_reloc_call(X86 *c, uint32_t target) {
    (void)c;
    (void)target;
    return 0;
}
int recomp_run_thunk(X86 *c, uint32_t target) {
    (void)c;
    harness_escape("thunk", target);
    return 0;
}
int d3d8_scene_boundary(X86 *c, uint32_t mode) {
    (void)c;
    harness_escape("d3d8", mode);
    return 0;
}
static jmp_buf seh_landing;

jmp_buf *recomp_seh_frame_enter(X86 *c) {
    (void)c;
    return &seh_landing;
}
jmp_buf *recomp_seh_frame_adopt(X86 *c) {
    harness_escape("seh", c->r[R_ESP]);
    return NULL;
}
void recomp_seh_frame_leave(X86 *c) {
    (void)c;
}
void recomp_seh_land(X86 *c) {
    harness_escape("seh", c->r[R_ESP]);
}
void recomp_seh_intercept(X86 *c, uint32_t target) {
    (void)c;
    harness_escape("seh", target);
}
uint64_t recomp_seh_frame_mark(X86 *c) {
    (void)c;
    return 0;
}
void recomp_seh_frame_orphan(X86 *c, uint64_t mark) {
    (void)c;
    (void)mark;
}
void recomp_execution_checkpoint_due(X86 *c) {
    (void)c;
}
uint32_t recomp_seh_pending_target(void) {
    return 0;
}
void recomp_saved_changed(X86 *c, uint32_t target, const RecompSaved *before) {
    (void)c;
    (void)before;
    harness_escape("saved", target);
}
void discovery_note(const char *kind, uint32_t target, uint32_t from) {
    (void)kind;
    (void)target;
    (void)from;
}
void discovery_write(void) {}
uint32_t recomp_guest_alloc(uint32_t size) {
    harness_escape("alloc", size);
    return 0;
}
void recomp_guest_free(uint32_t addr) {
    harness_escape("alloc", addr);
}
int recomp_file_alias_add(const char *guest_path, const char *host_path) {
    (void)guest_path;
    (void)host_path;
    harness_escape("file", 0);
    return 0;
}
void recomp_file_alias_remove(const char *guest_path) {
    (void)guest_path;
    harness_escape("file", 0);
}
int recomp_readable_path(const char *guest_path, char *out, size_t out_len) {
    (void)guest_path;
    (void)out;
    (void)out_len;
    harness_escape("file", 0);
    return 0;
}
int recomp_temp_file(const char *suffix, const void *data, size_t len, char *out, size_t out_len) {
    (void)suffix;
    (void)data;
    (void)len;
    (void)out;
    (void)out_len;
    harness_escape("file", 0);
    return 0;
}

typedef struct {
    uint32_t r[8];
    uint32_t eflags;
    uint16_t fpu_cw, fpu_sw, fpu_tag;
    uint32_t fpu_top;
    double st0;
    uint32_t escape_addr;
} DiffState;

static X86 diff_cpu;

const char *diff_run(uint32_t addr, DiffState *s, uint32_t fs_base) {
    X86 *c = &diff_cpu;
    memset(c, 0, sizeof *c);
    c->checkpoint_countdown = 1023u;
    memcpy(c->r, s->r, sizeof c->r);
    x86_set_eflags(c, s->eflags);
    x87_finit(c);
    x87_set_cw(c, s->fpu_cw);
    c->mxcsr = 0x1f80u;
    c->fs_base = fs_base;
    if (setjmp(escape_jb)) {
        s->escape_addr = escape_addr;
        return escape_reason;
    }
    escape_armed = 1;
    recomp_call(c, addr);
    escape_armed = 0;
    x86_cc_settle(c);
    memcpy(s->r, c->r, sizeof s->r);
    s->eflags = x86_get_eflags(c);
    s->fpu_cw = c->fpu_cw;
    s->fpu_sw = fstsw(c);
    s->fpu_tag = c->fpu_tag;
    s->fpu_top = c->fpu_top;
    s->st0 = c->st[c->fpu_top & 7];
    return NULL;
}
