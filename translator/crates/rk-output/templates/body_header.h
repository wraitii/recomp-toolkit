/* generated -- translated bodies do not depend on replacement policy */
#ifndef RECOMP_BODY_H
#define RECOMP_BODY_H
#include "x86.h"
#include "intrinsics.h"
#include "seh.h"
#define RECOMP_ENTRY_CAT_(a, b) a##b
#define RECOMP_ENTRY_CAT(a, b) RECOMP_ENTRY_CAT_(a, b)
#define CALL_FN(a) RECOMP_ENTRY_CAT(entry_, a)(c)
#define CALL_RAW(a) RECOMP_ENTRY_CAT(call_, a)(c)
#endif
