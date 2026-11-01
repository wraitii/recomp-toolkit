#pragma once
#include "pcode_core.h"
#include "x86.h"

static inline int64_t pcode_x87_integer(uint32_t addr, unsigned bits) {
    if (bits == 16)
        return (int16_t)rd16(addr);
    if (bits == 32)
        return (int32_t)rd32(addr);
    return (int64_t)rd64(addr);
}

static inline double pcode_x87_real(uint32_t addr, unsigned bits) {
    if (bits == 32)
        return rdf32(addr);
    if (bits == 64)
        return rdf64(addr);
    return rdf80(addr);
}

static inline void pcode_x87_push(X86 *c, unsigned a, unsigned bits, uint32_t addr) {
    if (a == 8)
        fpush(c, pcode_x87_real(addr, bits));
    else
        fpush_st(c, a);
}

static inline void pcode_x87_push_integer(X86 *c, unsigned bits, uint32_t addr) {
    fpush_int(c, pcode_x87_integer(addr, bits));
}

static inline void pcode_x87_store(X86 *c, unsigned a, unsigned bits, unsigned flags,
                                   unsigned extra, uint32_t addr) {
    if (a != 8)
        fcopy(c, a, 0);
    else if (extra) {
        if (bits == 16)
            wr16(addr, (uint16_t)fist_i16(c));
        else if (bits == 32)
            wr32(addr, (uint32_t)fist_i32(c));
        else
            wr64(addr, (uint64_t)fist_i64(c));
    } else if (bits == 32)
        wrf32(addr, fto_float(c, ST(c, 0)));
    else if (bits == 64)
        wrf64(addr, ST(c, 0));
    else
        wrf80(addr, ST(c, 0));
    if (flags)
        fdrop(c);
}

static inline void pcode_x87_arithmetic(X86 *c, unsigned a, unsigned bits, unsigned flags,
                                        unsigned extra, uint32_t addr) {
    unsigned dst = a & 15, src = a >> 4;
    double lhs = ST(c, dst), rhs = bits ? ((flags & 8) ? (double)pcode_x87_integer(addr, bits)
                                                       : pcode_x87_real(addr, bits))
                                        : ST(c, src);
    if (flags & 4) {
        double t = lhs;
        lhs = rhs;
        rhs = t;
    }
    double v;
    switch (flags & 3) {
    case 0:
        v = lhs + rhs;
        break;
    case 1:
        v = lhs - rhs;
        break;
    case 2:
        v = lhs * rhs;
        break;
    default:
        v = fdivz(c, lhs, rhs);
        break;
    }
    fset(c, dst, fx87(c, v));
    for (unsigned i = 0; i < extra; ++i)
        fdrop(c);
}

bool pcode_x87_supported(uint64_t id, const RpOp &op);
bool pcode_x87_execute(PcodeHost &host, X86 *c, uint64_t id, uint32_t input_count,
                       const uint64_t *args, uint64_t &result, std::string &why);
