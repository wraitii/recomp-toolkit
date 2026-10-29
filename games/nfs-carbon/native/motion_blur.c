/* DIVERGENCE(original): fade 0x726430's blur composite over 250 ms using
 * Speedbreaker flag 0xA9922C (0x693C50). Preserve 0x7133C0's state updates;
 * scale its +0xB8 composite strength temporarily, then restore shared state. */
#include "motion_blur.h"
#include "../../../platform/os.h"
#include <stdlib.h>
#include <string.h>

void fn_007133c0(X86 *c);
void fn_00726430(X86 *c);

typedef struct {
    float fade;
    uint32_t strength_address;
    uint32_t strength;
} BlurPass;

static BlurPass *blur_pass;

static int original_behavior(void) {
    static int original = -1;
    if (original < 0) {
        const char *value = getenv("RECOMP_MOTION_BLUR_ORIGINAL");
        original = value && strcmp(value, "1") == 0;
    }
    return original;
}

void nfs_carbon_motion_blur(X86 *c) {
    uint32_t effect = c->r[R_ECX];
    fn_007133c0(c);
    if (original_behavior())
        return;
    float fade = blur_pass ? blur_pass->fade : (rd8(0x00a9922cu) ? 1.0f : 0.0f);
    if (blur_pass) {
        blur_pass->strength_address = effect + 0xb8u;
        blur_pass->strength = rd32(blur_pass->strength_address);
        wrf32(blur_pass->strength_address, rdf32(blur_pass->strength_address) * fade);
    }
    if (fade == 0.0f)
        c->r[R_EAX] &= 0xffffff00u;
}

void nfs_carbon_render_motion_blur(X86 *c) {
    static uint64_t last_time;
    static float fade;
    if (original_behavior()) {
        fn_00726430(c);
        return;
    }
    uint64_t now = os_monotonic_ns();
    float step = last_time ? (float)((double)(now - last_time) / 250000000.0) : 0.0f;
    last_time = now;
    if (rd8(0x00a9922cu)) {
        fade += step;
        if (fade > 1.0f)
            fade = 1.0f;
    } else {
        fade -= step;
        if (fade < 0.0f)
            fade = 0.0f;
    }
    BlurPass pass = {fade, 0, 0};
    BlurPass *previous = blur_pass;
    blur_pass = &pass;
    fn_00726430(c);
    blur_pass = previous;
    if (pass.strength_address)
        wr32(pass.strength_address, pass.strength);
}
